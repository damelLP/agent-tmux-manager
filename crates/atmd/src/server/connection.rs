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

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter,
};
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
use atm_protocol::{error_code, ClientMessage, DaemonMessage, MessageType, ProtocolVersion};

use super::methods::{CallContext, CallerId, CallerPane, MethodRouter};
use super::MAX_TUI_CLIENTS;
use crate::discovery::{DiscoveryResult, DiscoveryService};
use crate::registry::{RegistryHandle, SessionEvent};

/// Type alias for subscriber writer handle
pub type SubscriberWriter<W = OwnedWriteHalf> = Arc<Mutex<BufWriter<W>>>;

/// Information about a subscribed client
pub struct Subscriber<W = OwnedWriteHalf> {
    /// Writer for sending events
    pub writer: SubscriberWriter<W>,

    /// Optional filter for session-specific subscriptions
    pub filter: Option<SessionId>,
}

/// Type alias for the subscribers map, keyed by [`subscriber_key`]
pub type SubscribersMap<W = OwnedWriteHalf> = Arc<RwLock<HashMap<String, Subscriber<W>>>>;

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

/// Exhaustive so new envelopes must explicitly choose whether they need a reply.
fn is_one_way(message: &MessageType) -> bool {
    match message {
        MessageType::HookEvent { .. }
        | MessageType::StatusUpdate { .. }
        | MessageType::PiEvent { .. }
        | MessageType::CodexEvent { .. } => true,
        MessageType::Connect { .. }
        | MessageType::ListSessions
        | MessageType::Subscribe { .. }
        | MessageType::Unsubscribe
        | MessageType::Ping { .. }
        | MessageType::Disconnect
        | MessageType::Discover
        | MessageType::Request { .. }
        | MessageType::Watch { .. }
        | MessageType::Unwatch => false,
    }
}

/// Connection handler for a single client.
///
/// Manages the lifecycle of a client connection including:
/// - Protocol handshake
/// - Message processing loop
/// - Event subscription (for TUI clients)
/// - Graceful shutdown
pub struct ConnectionHandler<R = OwnedReadHalf, W = OwnedWriteHalf> {
    /// Buffered reader for incoming messages
    reader: BufReader<R>,

    /// Buffered writer for outgoing messages (shared for event broadcast)
    writer: SubscriberWriter<W>,

    /// Handle to the session registry
    registry: RegistryHandle,

    /// Shared subscribers map for event broadcasting
    subscribers: SubscribersMap<W>,

    /// Unique client identifier (assigned after handshake)
    client_id: Option<ClientId>,

    /// Connect was accepted, but its reply failed: drain only one-way events.
    one_way_only: bool,

    /// Whether this client is subscribed to events
    subscribed: bool,

    /// Session ID filter for subscriptions (None = all sessions)
    subscription_filter: Option<SessionId>,

    /// Counter for generating client IDs
    connection_number: u64,

    /// Cancelled on server shutdown; aborts even accepted requests.
    cancel: CancellationToken,

    /// A broken peer stops input/replies, but accepted handlers still finish.
    peer_failed: CancellationToken,

    /// Forwards registry events while subscribed
    forwarder: Option<Forwarder>,

    methods: Arc<MethodRouter>,
    raw: crate::org::RawHandle,
    org: crate::org::OrgHandle,
    message_evidence: Option<crate::org::message_evidence::MessageEvidence>,
    message_history: Option<crate::org::message_history::MessageHistoryHandle>,
    watcher: Option<Forwarder>,
}

impl<R, W> ConnectionHandler<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
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
    /// * `methods` - Shared, immutable control-plane method registry
    pub fn new(
        reader: R,
        writer: W,
        registry: RegistryHandle,
        subscribers: SubscribersMap<W>,
        connection_number: u64,
        cancel: CancellationToken,
        methods: Arc<MethodRouter>,
    ) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer: Arc::new(Mutex::new(BufWriter::new(writer))),
            registry,
            subscribers,
            client_id: None,
            one_way_only: false,
            subscribed: false,
            subscription_filter: None,
            connection_number,
            cancel,
            peer_failed: CancellationToken::new(),
            forwarder: None,
            methods,
            raw: crate::org::RawHandle::default(),
            org: crate::org::OrgHandle::disabled("organisation service not configured"),
            message_evidence: None,
            message_history: None,
            watcher: None,
        }
    }

    pub fn with_raw(mut self, raw: crate::org::RawHandle) -> Self {
        self.raw = raw;
        self
    }

    pub fn with_org(mut self, org: crate::org::OrgHandle) -> Self {
        self.org = org;
        self
    }

    /// Inject the daemon's shared evidence and optional initialized history.
    /// The tap never waits for the message service or organisation writer.
    pub fn with_message_evidence(
        mut self,
        evidence: Option<crate::org::message_evidence::MessageEvidence>,
        history: Option<crate::org::message_history::MessageHistoryHandle>,
    ) -> Self {
        self.message_evidence = evidence;
        self.message_history = history;
        self
    }

    /// Returns a clone of the writer for event broadcasting.
    pub fn writer_handle(&self) -> SubscriberWriter<W> {
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
                    one_way_only = self.one_way_only,
                    "Client Connect accepted"
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
                let reply = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => return Err(ConnectionError::Eof),
                    reply = self.send_message(DaemonMessage::Connected {
                        protocol_version: ProtocolVersion::CURRENT,
                        client_id: assigned_id,
                        methods: self.methods.names(),
                    }) => reply,
                };
                if let Err(error) = reply {
                    if !matches!(
                        error,
                        ConnectionError::Io(_) | ConnectionError::WriteTimeout
                    ) {
                        return Err(error);
                    }
                    debug!(client_id = ?self.client_id, %error, "Handshake reply failed; draining one-way events");
                    // Safe because Connect is fully validated before its reply;
                    // revisit if the handshake ever requires a round trip.
                    // New writes outside handle_message must also check this flag.
                    self.one_way_only = true;
                }

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
        let mut requests = JoinSet::new();
        let result = self.process_message_loop(&mut requests).await;
        self.stop_forwarder().await;
        self.stop_watcher().await;
        let mut writable = result.is_ok() && !self.peer_failed.is_cancelled();
        // EOF/disconnect closes only input. Finish accepted work up to its own
        // deadline, even if the peer can no longer receive replies. Only daemon
        // shutdown aborts the remaining jobs by dropping this JoinSet.
        while !requests.is_empty() {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                done = requests.join_next() => match done {
                    Some(Ok(response)) if writable => {
                        if let Err(error) = self.send_message(response).await {
                            debug!(%error, "Stopped replying while draining requests");
                            writable = false;
                        }
                    }
                    Some(Err(error)) => warn!(%error, "Request task failed while draining"),
                    Some(Ok(_)) => {},
                    None => break,
                }
            }
        }
        result
    }

    async fn process_message_loop(
        &mut self,
        requests: &mut JoinSet<DaemonMessage>,
    ) -> Result<(), ConnectionError> {
        let cancel = self.cancel.clone();
        let peer_failed = self.peer_failed.clone();
        let writer = Arc::clone(&self.writer);
        loop {
            let subscribed = self.subscribed;
            let watch_stop = self
                .watcher
                .as_ref()
                .map(|watcher| watcher.stop.clone())
                .unwrap_or_default();
            let mut watching = self.watcher.is_some() && !watch_stop.is_cancelled();
            let mut idle_at = tokio::time::Instant::now() + READ_TIMEOUT;
            let read = {
                // Keep this future alive across job completions: read_message
                // holds a partial line locally and is NOT cancellation-safe.
                let read = self.read_message();
                tokio::pin!(read);
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Ok(()),
                        _ = peer_failed.cancelled() => return Err(ConnectionError::WriteTimeout),
                        Some(done) = requests.join_next(), if !requests.is_empty() => {
                            let response = done.map_err(|e| ConnectionError::RequestTask(e.to_string()))?;
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => return Ok(()),
                                result = write_message(&writer, &response) => result?,
                            }
                            idle_at = tokio::time::Instant::now() + READ_TIMEOUT;
                        }
                        read = &mut read => break read,
                        _ = watch_stop.cancelled(), if watching => {
                            watching = false;
                            idle_at = tokio::time::Instant::now() + READ_TIMEOUT;
                        }
                        _ = tokio::time::sleep_until(idle_at), if !subscribed && !watching && requests.is_empty() => {
                            return Err(ConnectionError::Timeout);
                        }
                    }
                }
            };
            let msg = match read {
                Ok(msg) => msg,
                Err(ConnectionError::Eof) => return Ok(()),
                Err(e) => return Err(e),
            };

            // Process the message
            if let Err(e) = self.handle_message(msg, requests).await {
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
                if matches!(
                    e,
                    ConnectionError::WriteTimeout
                        | ConnectionError::Io(_)
                        | ConnectionError::RequestTask(_)
                ) {
                    return Err(e);
                }

                // Send error response but continue processing, unless the
                // reply itself failed
                if !self.one_way_only {
                    self.send_message(DaemonMessage::error(&e.to_string()))
                        .await?;
                }
            }
        }
    }

    /// Handles a single client message.
    async fn handle_message(
        &mut self,
        msg: ClientMessage,
        requests: &mut JoinSet<DaemonMessage>,
    ) -> Result<(), ConnectionError> {
        if self.one_way_only && !is_one_way(&msg.message) {
            debug!(client_id = ?self.client_id, message_type = ?std::mem::discriminant(&msg.message), "Skipping message after failed handshake reply");
            return Ok(());
        }

        match msg.message {
            MessageType::Connect { .. } => {
                // Already connected - send error
                self.send_message(DaemonMessage::error("Already connected"))
                    .await?;
            }

            MessageType::Request {
                id,
                method,
                params,
                from_pane,
            } => {
                while let Some(done) = requests.try_join_next() {
                    let response = done.map_err(|e| ConnectionError::RequestTask(e.to_string()))?;
                    self.send_message(response).await?;
                }
                // The tail payload budget leaves room for ordinary IDs. A
                // pathological ID can still push the complete line over 1 MiB.
                // Check before admission too: overload errors must fit as well.
                let tail_refusal = if method == "log.tail" {
                    let refusal = DaemonMessage::response_err(
                        id.clone(),
                        error_code::REFUSED,
                        "log.tail response exceeds 1 MiB; use a shorter id or smaller limit",
                    );
                    if !fits_response(&refusal) {
                        return Err(ConnectionError::Io(
                            "log.tail request id leaves no room for a response".into(),
                        ));
                    }
                    Some(refusal)
                } else {
                    None
                };
                if requests.len() >= 64 {
                    self.send_message(DaemonMessage::response_err(
                        id,
                        error_code::UNAVAILABLE,
                        "too many in-flight requests (max: 64)",
                    ))
                    .await?;
                    return Ok(());
                }
                let context = CallContext {
                    client_id: CallerId(self.client_id.clone().ok_or_else(|| {
                        ConnectionError::UnexpectedMessage("request before handshake".into())
                    })?),
                    from_pane: from_pane.map(CallerPane),
                };
                let methods = Arc::clone(&self.methods);
                requests.spawn(async move {
                    let response = match methods.call(context, &method, params).await {
                        Ok(value) => DaemonMessage::response_ok(id, value),
                        Err(error) => {
                            DaemonMessage::response_err(id, &error.0.code, &error.0.message)
                        }
                    };
                    match tail_refusal {
                        Some(refusal) if !fits_response(&response) => refusal,
                        _ => response,
                    }
                });
            }
            MessageType::Watch { topics, cursor } => {
                self.stop_watcher().await;
                match self.org.watch(topics, cursor) {
                    Ok(watch) => {
                        self.watcher = Some(Forwarder::watch(
                            watch,
                            Arc::clone(&self.writer),
                            &self.peer_failed,
                        ));
                    }
                    Err(error) => {
                        self.send_message(DaemonMessage::error_with_code(
                            &error.to_string(),
                            error_code::UNAVAILABLE,
                        ))
                        .await?
                    }
                }
            }
            MessageType::Unwatch => self.stop_watcher().await,

            MessageType::StatusUpdate { data } => {
                self.raw
                    .capture(chrono::Utc::now(), "status_update", data.clone());
                self.handle_status_update(data).await?;
            }

            MessageType::HookEvent { data } => {
                self.raw
                    .capture(chrono::Utc::now(), "hook_event", data.clone());
                self.handle_hook_event(data).await?;
            }

            MessageType::PiEvent { data } => {
                self.raw.capture(
                    chrono::Utc::now(),
                    "pi_event",
                    atm_pi_adapter::wire::raw_log_body(&data),
                );
                self.handle_pi_event(data).await?;
            }

            MessageType::CodexEvent { data } => {
                self.raw
                    .capture(chrono::Utc::now(), "codex_event", data.clone());
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
                    &self.peer_failed,
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
        // serde would also fill the line from an array, by field position
        if !data.is_object() {
            return Err(ConnectionError::ParseError(
                "status line is not a JSON object".to_string(),
            ));
        }
        let (pid, status) = parse_status_line(data)?;
        let session_id = SessionId::new(status.session_id.clone());

        // Send to registry for processing
        let applied = self
            .registry
            .update_from_status_line(
                session_id.clone(),
                pid,
                status,
                atm_core::Harness::ClaudeCode,
            )
            .await;
        // Status lines have no original pane: activity only, never repair one
        // from the current row to manufacture prompt evidence. Activity can
        // neither bind nor confirm, so it needs no parent proof.
        if self.message_tapped() {
            self.record_message_event(&session_id, None, None, None, false);
        }
        applied.map_err(|e| ConnectionError::RegistryError(e.to_string()))
    }

    /// Evidence is installed and usable. A disabled one costs a hook nothing.
    fn message_tapped(&self) -> bool {
        self.message_evidence
            .as_ref()
            .is_some_and(|evidence| evidence.available())
    }

    /// Apply once and tap only parent events. Generated metadata never uses
    /// this entry point. A tapped event is never dropped: when its apply fails
    /// or its parent cannot be proven, evidence takes it as unproven.
    async fn apply_message_lifecycle(
        &self,
        session: SessionId,
        event: atm_core::LifecycleEvent,
        harness: atm_core::Harness,
        pid: Option<u32>,
        pane: Option<String>,
        context: atm_core::LifecycleContext,
    ) -> Result<(), ConnectionError> {
        let tapped = context.child.is_none()
            && !matches!(
                event,
                atm_core::LifecycleEvent::ChildSessionStart { .. }
                    | atm_core::LifecycleEvent::ChildSessionEnd { .. }
            )
            && self.message_tapped();
        // A SessionEnd removes its row, so no snapshot can prove it afterwards.
        // It needs none: it is activity of a session that can bind no more.
        let ends = matches!(event, atm_core::LifecycleEvent::SessionEnd { .. });
        let applied = self
            .registry
            .apply_lifecycle_event_with_context(
                session.clone(),
                event.clone(),
                harness,
                pid,
                pane.clone(),
                context,
            )
            .await;
        if tapped {
            let proven = applied.is_ok()
                && !ends
                && self
                    .message_parent_proven(&session, pane.as_deref(), harness)
                    .await;
            self.record_message_event(
                &session,
                pane.as_deref(),
                Some(harness),
                Some(&event),
                proven,
            );
        }
        applied.map_err(|e| ConnectionError::RegistryError(e.to_string()))
    }

    /// The proof a receipt or a lazy binding needs: one healthy snapshot shows
    /// exactly one parent row for the event's own session, harness and pane,
    /// and no other parent row on that pane. An Ok from the registry can mean
    /// an ignored event or PID routing into another row, and an empty snapshot
    /// can come from a dying actor, so anything else proves nothing. Never use
    /// first_prompt, and never borrow the row's identity for the event.
    async fn message_parent_proven(
        &self,
        session: &SessionId,
        pane: Option<&str>,
        harness: atm_core::Harness,
    ) -> bool {
        // Without a pane no event binds or confirms: there is nothing to prove.
        let Some(pane) = pane else {
            return false;
        };
        let Ok(rows) = timeout(Duration::from_secs(1), self.registry.get_all_sessions()).await
        else {
            warn!(session_id = %session, "Registry snapshot timed out; message event is unproven");
            return false;
        };
        let on_pane = |row: &&atm_core::SessionView| {
            row.parent_session_id.is_none() && row.tmux_pane.as_deref() == Some(pane)
        };
        let mut own = rows
            .iter()
            .filter(on_pane)
            .filter(|row| row.id == *session && row.harness == harness.short_tag());
        own.next().is_some()
            && own.next().is_none()
            && !rows.iter().filter(on_pane).any(|row| row.id != *session)
    }

    /// The one neutral tap, after the registry's answer. A status line passes
    /// no harness and no event: activity only.
    fn record_message_event(
        &self,
        session: &SessionId,
        pane: Option<&str>,
        harness: Option<atm_core::Harness>,
        event: Option<&atm_core::LifecycleEvent>,
        proven: bool,
    ) {
        let Some(evidence) = &self.message_evidence else {
            return;
        };
        // An unparsable pane is an unknown pane. Without a pane no event can
        // bind or confirm, whatever its proof.
        let pane = pane.filter(|p| crate::control::reservation::PaneId::parse(*p).is_ok());
        let observed = evidence.tap(
            session,
            pane,
            harness,
            event,
            proven,
            self.message_history.as_ref(),
        );
        if let Err(error) = observed {
            // Evidence logs its own disable once; the registry event stays applied.
            debug!(%error, "Message evidence did not take the event");
        }
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
        self.apply_message_lifecycle(
            session_id,
            lifecycle,
            atm_core::Harness::ClaudeCode,
            pid,
            tmux_pane,
            raw_event.lifecycle_context(),
        )
        .await?;

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

        self.apply_message_lifecycle(
            session_id,
            lifecycle,
            atm_core::Harness::Pi,
            raw_event.pid,
            raw_event.tmux_pane,
            Default::default(),
        )
        .await?;

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

        self.apply_message_lifecycle(
            session_id.clone(),
            lifecycle,
            atm_core::Harness::Codex,
            pid,
            tmux_pane.clone(),
            Default::default(),
        )
        .await?;

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
            // Unsubscribe/disconnect preserves frame boundaries. Shutdown
            // closes the socket, so dropping a partial write is safe and must
            // not wait for a stuck reader's write deadline.
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {},
                _ = forwarder.stop() => {},
            }
        }
    }

    async fn stop_watcher(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {},
                _ = watcher.stop() => {},
            }
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
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(ConnectionError::Eof),
            result = write_message(&self.writer, &msg) => result,
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

    #[error("Request task failed: {0}")]
    RequestTask(String),
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
    fn spawn<W: AsyncWrite + Unpin + Send + 'static>(
        client_id: ClientId,
        events: broadcast::Receiver<SessionEvent>,
        writer: SubscriberWriter<W>,
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

    fn watch<W: AsyncWrite + Unpin + Send + 'static>(
        mut events: crate::org::EventWatch,
        writer: SubscriberWriter<W>,
        connection: &CancellationToken,
    ) -> Self {
        let stop = connection.child_token();
        let mut task = JoinSet::new();
        let stopped = stop.clone();
        let connection = connection.clone();
        task.spawn(async move {
            let _done = stopped.clone().drop_guard();
            loop {
                let event = tokio::select! {
                    biased;
                    _ = stopped.cancelled() => break,
                    event = events.next() => event,
                };
                let (message, done) = match event {
                    Ok(Some(event)) => (crate::org::event_message(&event), false),
                    Ok(None) => break,
                    Err(error) => (watch_error(error), true),
                };
                // Unwatch waits at frame boundaries. Shutdown drops the owning
                // JoinSet, cancelling even a blocked write to this peer.
                if write_message(&writer, &message).await.is_err() {
                    connection.cancel();
                    break;
                }
                if done {
                    break;
                }
            }
        });
        Self { stop, task }
    }

    /// Stops forwarding and waits for the task. The task only checks
    /// `stop` between events, so it never leaves half a line behind.
    async fn stop(mut self) {
        self.stop.cancel();
        while self.task.join_next().await.is_some() {}
    }
}

fn watch_error(error: crate::org::OrgServiceError) -> DaemonMessage {
    let code = match error {
        crate::org::OrgServiceError::Timeout => error_code::TIMEOUT,
        _ => error_code::UNAVAILABLE,
    };
    DaemonMessage::error_with_code(&error.to_string(), code)
}

/// Forwards registry events to one subscriber until `stop` is cancelled.
///
/// Each subscription has its own broadcast receiver, so a slow subscriber
/// only delays itself. One that can't keep up has its whole `connection`
/// cancelled: a write that exceeded [`WRITE_TIMEOUT`] may have left half a
/// line on the socket, and a receiver that lagged past the channel's
/// capacity has missed events. The client recovers by reconnecting, which
/// re-subscribes and fetches a full session list.
async fn forward_events<W: AsyncWrite + Unpin>(
    client_id: ClientId,
    mut events: broadcast::Receiver<SessionEvent>,
    writer: SubscriberWriter<W>,
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
pub async fn send_event<W: AsyncWrite + Unpin>(
    writer: &SubscriberWriter<W>,
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

    write_message(writer, &msg).await
}

fn fits_response(message: &DaemonMessage) -> bool {
    serde_json::to_vec(message).is_ok_and(|bytes| bytes.len() < MAX_MESSAGE_SIZE)
}

/// Serializes one complete reply under the connection's shared writer lock.
async fn write_message<W: AsyncWrite + Unpin>(
    writer: &SubscriberWriter<W>,
    msg: &DaemonMessage,
) -> Result<(), ConnectionError> {
    let json =
        serde_json::to_string(msg).map_err(|e| ConnectionError::ParseError(e.to_string()))?;

    match timeout(WRITE_TIMEOUT, async {
        let mut writer = writer.lock().await;
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

    fn handshake_pair() -> (ConnectionHandler, UnixStream) {
        let (ours, peer) = UnixStream::pair().unwrap();
        let (read, write) = ours.into_split();
        let handler = ConnectionHandler::new(
            read,
            write,
            crate::registry::spawn_registry(),
            Arc::new(RwLock::new(HashMap::new())),
            1,
            CancellationToken::new(),
            Arc::new(MethodRouter::default()),
        );
        (handler, peer)
    }

    fn handshake_hook() -> ClientMessage {
        // Synthetic: only fields needed to create a session; no harness is run.
        ClientMessage::hook_event(serde_json::json!({
            "session_id": "closed-hook", "hook_event_name": "SessionStart",
            "pid": std::process::id()
        }))
    }

    async fn queue_messages(peer: &mut UnixStream, messages: Vec<ClientMessage>) {
        for message in messages {
            timeout(
                Duration::from_secs(30),
                peer.write_all(
                    format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }

    #[tokio::test]
    async fn closed_hook_registers_after_failed_connected_reply() {
        timeout(Duration::from_secs(30), async {
            let (handler, mut peer) = handshake_pair();
            let registry = handler.registry.clone();
            queue_messages(
                &mut peer,
                vec![ClientMessage::connect(None), handshake_hook()],
            )
            .await;
            // Full close, before run: Connected must fail, but the event is readable.
            drop(peer);
            handler.run().await;
            assert!(
                registry
                    .get_session(SessionId::new("closed-hook"))
                    .await
                    .is_some(),
                "buffered SessionStart was dropped after failed handshake reply"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn one_way_drain_applies_later_events_in_order() {
        timeout(Duration::from_secs(30), async {
            let (handler, mut peer) = handshake_pair();
            let registry = handler.registry.clone();
            // Synthetic: the second event changes the same session from idle to working.
            let prompt = ClientMessage::hook_event(serde_json::json!({
                "session_id": "closed-hook", "hook_event_name": "UserPromptSubmit",
                "pid": std::process::id()
            }));
            queue_messages(
                &mut peer,
                vec![ClientMessage::connect(None), handshake_hook(), prompt],
            )
            .await;
            drop(peer);
            handler.run().await;
            let row = registry
                .get_session(SessionId::new("closed-hook"))
                .await
                .unwrap();
            assert_eq!(
                row.status_label, "working",
                "second queued event was not drained"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn closed_hook_payload_is_valid_and_buffered() {
        timeout(Duration::from_secs(30), async {
            let (mut handler, mut peer) = handshake_pair();
            queue_messages(
                &mut peer,
                vec![ClientMessage::connect(None), handshake_hook()],
            )
            .await;
            drop(peer);
            let error = handler
                .send_message(DaemonMessage::pong(0))
                .await
                .unwrap_err();
            assert!(
                matches!(error, ConnectionError::Io(ref message) if message.contains("Broken pipe"))
            );
            assert!(matches!(
                handler.read_message().await.unwrap().message,
                MessageType::Connect { .. }
            ));
            let event = handler.read_message().await.unwrap();
            handler
                .handle_message(event, &mut JoinSet::new())
                .await
                .unwrap();
            assert!(handler
                .registry
                .get_session(SessionId::new("closed-hook"))
                .await
                .is_some());
            assert!(matches!(
                handler.read_message().await,
                Err(ConnectionError::Eof)
            ));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_connected_reply_drains_all_one_way_envelopes() {
        timeout(Duration::from_secs(30), async {
            // Synthetic: minimal payloads using the shapes in server_integration.rs.
            let pid = std::process::id();
            let cases = [
                (handshake_hook(), "claude"),
                (
                    ClientMessage::status_update(serde_json::json!({
                        "session_id": "closed-hook", "pid": pid,
                        "model": {"id": "claude-sonnet-4-20250514"}
                    })),
                    "claude",
                ),
                (
                    ClientMessage::pi_event(serde_json::json!({
                        "session_id": "closed-hook", "pid": pid,
                        "event": "session_start", "payload": {"type": "session_start"}
                    })),
                    "pi",
                ),
                (
                    ClientMessage::codex_event(serde_json::json!({
                        "session_id": "closed-hook", "pid": pid,
                        "hook_event_name": "SessionStart", "source": "startup"
                    })),
                    "codex",
                ),
            ];
            for (event, harness) in cases {
                let (handler, mut peer) = handshake_pair();
                let registry = handler.registry.clone();
                // Batch multiple one-way envelopes on the same closed connection.
                queue_messages(
                    &mut peer,
                    vec![ClientMessage::connect(None), event.clone(), event],
                )
                .await;
                drop(peer);
                handler.run().await;
                let row = registry
                    .get_session(SessionId::new("closed-hook"))
                    .await
                    .unwrap();
                assert_eq!(row.harness, harness);
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_connected_reply_skips_controls_without_starting_work() {
        timeout(Duration::from_secs(30), async {
            let (mut handler, mut peer) = handshake_pair();
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = calls.clone();
            let mut router = MethodRouter::default();
            router
                .register("count", Duration::from_secs(30), move |_, _| {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async { Ok(serde_json::Value::Null) }
                })
                .unwrap();
            handler.methods = Arc::new(router);
            queue_messages(
                &mut peer,
                vec![
                    ClientMessage::connect(None),
                    ClientMessage::request(
                        "1".into(),
                        "count".into(),
                        serde_json::Value::Null,
                        None,
                    ),
                    ClientMessage::subscribe(None),
                    ClientMessage::watch(vec![], 0),
                    ClientMessage::list_sessions(),
                    ClientMessage::ping(1),
                    ClientMessage::discover(),
                    ClientMessage::connect(None),
                    ClientMessage::new(MessageType::Unsubscribe),
                    ClientMessage::unwatch(),
                    ClientMessage::disconnect(),
                    handshake_hook(),
                ],
            )
            .await;
            drop(peer);
            handler.handle_handshake().await.unwrap();
            let mut requests = JoinSet::new();
            handler.process_message_loop(&mut requests).await.unwrap();
            assert!(requests.is_empty(), "a request was admitted");
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(handler.subscribers.read().await.is_empty());
            assert!(!handler.subscribed);
            assert!(handler.forwarder.is_none());
            assert!(handler
                .registry
                .get_session(SessionId::new("closed-hook"))
                .await
                .is_some());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn one_way_drain_never_reacquires_reply_writer() {
        timeout(Duration::from_secs(30), async {
            let (mut handler, mut peer) = handshake_pair();
            queue_messages(
                &mut peer,
                vec![
                    ClientMessage::connect(None),
                    ClientMessage::ping(1),
                    ClientMessage::hook_event(serde_json::Value::Null),
                    ClientMessage::status_update(serde_json::Value::Null),
                    ClientMessage::pi_event(serde_json::Value::Null),
                    ClientMessage::codex_event(serde_json::Value::Null),
                    handshake_hook(),
                ],
            )
            .await;
            drop(peer);
            handler.handle_handshake().await.unwrap();
            let writer = handler.writer_handle();
            let _held = writer.lock().await;
            handler.process_messages().await.unwrap();
            assert!(handler
                .registry
                .get_session(SessionId::new("closed-hook"))
                .await
                .is_some());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rejected_connect_does_not_drain_events() {
        timeout(Duration::from_secs(30), async {
            let mut incompatible = ClientMessage::connect(None);
            incompatible.protocol_version = ProtocolVersion::new(99, 0);
            let cases = [
                serde_json::to_string(&incompatible).unwrap(),
                "not json".into(),
                serde_json::to_string(&ClientMessage::ping(1)).unwrap(),
            ];
            for (case, first) in cases.iter().enumerate() {
                for closed in [false, true] {
                    let (handler, mut peer) = handshake_pair();
                    let registry = handler.registry.clone();
                    peer.write_all(format!("{first}\n").as_bytes())
                        .await
                        .unwrap();
                    queue_messages(&mut peer, vec![handshake_hook()]).await;
                    if closed {
                        drop(peer);
                        assert!(handler.run().await.is_none());
                    } else {
                        peer.shutdown().await.unwrap();
                        assert!(handler.run().await.is_none());
                        let mut replies = String::new();
                        peer.read_to_string(&mut replies).await.unwrap();
                        match case {
                            0 => assert!(matches!(
                                serde_json::from_str::<DaemonMessage>(&replies).unwrap(),
                                DaemonMessage::Rejected { .. }
                            )),
                            1 => assert!(replies.is_empty()),
                            _ => assert!(matches!(
                                serde_json::from_str::<DaemonMessage>(&replies).unwrap(),
                                DaemonMessage::Error { .. }
                            )),
                        }
                    }
                    assert!(registry
                        .get_session(SessionId::new("closed-hook"))
                        .await
                        .is_none());
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn successful_handshake_still_services_controls() {
        timeout(Duration::from_secs(30), async {
            let (handler, mut peer) = handshake_pair();
            queue_messages(
                &mut peer,
                vec![
                    ClientMessage::connect(None),
                    ClientMessage::subscribe(None),
                    ClientMessage::ping(23),
                ],
            )
            .await;
            // Half-close is not full close: queued controls still receive replies.
            peer.shutdown().await.unwrap();
            let run = tokio::spawn(handler.run());
            let (read, _write) = peer.into_split();
            let mut read = BufReader::new(read);
            assert!(matches!(
                receive(&mut read).await,
                DaemonMessage::Connected { .. }
            ));
            assert!(matches!(
                receive(&mut read).await,
                DaemonMessage::SessionList { .. }
            ));
            assert!(matches!(
                receive(&mut read).await,
                DaemonMessage::Pong { seq: 23 }
            ));
            assert!(run.await.unwrap().is_some());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_handshake_reply_does_not_override_shutdown() {
        timeout(Duration::from_secs(30), async {
            let (handler, mut peer) = handshake_pair();
            let registry = handler.registry.clone();
            queue_messages(
                &mut peer,
                vec![ClientMessage::connect(None), handshake_hook()],
            )
            .await;
            drop(peer);
            handler.cancel.cancel();
            assert!(handler.run().await.is_none());
            assert!(registry
                .get_session(SessionId::new("closed-hook"))
                .await
                .is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancellation_during_connected_reply_does_not_enter_one_way_mode() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;

        timeout(Duration::from_secs(30), async {
            let (mut handler, mut peer) = handshake_pair();
            queue_messages(&mut peer, vec![ClientMessage::connect(None)]).await;
            // Prime the real socket reader before polling the reply future.
            handler.reader.fill_buf().await.unwrap();
            let writer = handler.writer_handle();
            let _held = writer.lock().await;
            let cancel = handler.cancel.clone();
            {
                let handshake = handler.handle_handshake();
                tokio::pin!(handshake);
                poll_fn(|cx| {
                    assert!(handshake.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                // The handshake has now reached the held writer mutex.
                cancel.cancel();
                assert!(matches!(handshake.await, Err(ConnectionError::Eof)));
            }
            assert!(!handler.one_way_only);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connected_reply_timeout_enters_one_way_mode() {
        let (mut handler, mut peer) = handshake_pair();
        queue_messages(
            &mut peer,
            vec![ClientMessage::connect(None), handshake_hook()],
        )
        .await;
        drop(peer);
        // Prime the reader before pausing: no real socket I/O is pending when
        // the clock auto-advances to the deadline on the held writer mutex.
        timeout(Duration::from_secs(30), handler.reader.fill_buf())
            .await
            .unwrap()
            .unwrap();
        let writer = handler.writer_handle();
        let held = writer.lock().await;
        tokio::time::pause();
        handler.handle_handshake().await.unwrap();
        tokio::time::resume();
        drop(held);
        timeout(Duration::from_secs(30), handler.process_messages())
            .await
            .unwrap()
            .unwrap();
        assert!(handler
            .registry
            .get_session(SessionId::new("closed-hook"))
            .await
            .is_some());
    }

    // Synthetic: identity/reference mutations of the captured transport shapes.
    // Captured: Claude Code 2.1.310, 2026-10-24, TUI hook/status capture.
    // Captured: pi 0.73.1, 2026-10-24, TUI extension frames.
    // Captured: Codex 0.163.0, 2026-10-24, TUI hook stdin.
    // Each adapter's crate docs record the source fixture and capture method.
    type EvidenceHandler = ConnectionHandler<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >;
    async fn evidence_handler(
        harness: atm_core::Harness,
    ) -> (
        EvidenceHandler,
        crate::org::message_evidence::MessageEvidence,
    ) {
        use crate::org::{
            message_evidence::{AttemptScope, MessageEvidence},
            messages::MessageClock,
        };
        let evidence = MessageEvidence::new(MessageClock::new(
            tokio::time::Instant::now(),
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        ));
        let id = "m123".parse().unwrap();
        evidence
            .register(id, "[m123] Read: atm inbox --id m123".into())
            .unwrap();
        evidence
            .arm(AttemptScope {
                message_id: id,
                attempt_seq: 124,
                recipient: "demo/worker".parse().unwrap(),
                pane: crate::control::reservation::PaneId::parse("%7").unwrap(),
                harness,
                session_id: Some(SessionId::new("evidence-session")),
                excluded_sessions: Default::default(),
            })
            .unwrap();
        let registry = crate::registry::spawn_registry();
        let mut session = SessionDomain::new(
            SessionId::new("evidence-session"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        session.harness = harness;
        session.tmux_pane = Some("%7".into());
        registry.register(session).await.unwrap();
        let (ours, _peer) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(ours);
        let handler = ConnectionHandler::new(
            read,
            write,
            registry,
            Arc::new(RwLock::new(HashMap::new())),
            1,
            CancellationToken::new(),
            request_router(),
        )
        .with_message_evidence(Some(evidence.clone()), None);
        (handler, evidence)
    }

    fn captured_prompt(harness: atm_core::Harness) -> serde_json::Value {
        use atm_core::Harness;
        let text = match harness {
            Harness::ClaudeCode => include_str!(
                "../../../atm-claude-adapter/tests/fixtures/claude_tui_session_2.1.310.jsonl"
            ),
            Harness::Pi => {
                include_str!("../../../atm-pi-adapter/tests/fixtures/pi_tool_run_0.73.1.jsonl")
            }
            Harness::Codex => include_str!(
                "../../../atm-codex-adapter/tests/fixtures/codex_tui_session_0.163.0.jsonl"
            ),
            _ => unreachable!(),
        };
        let mut payload = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .map(|value| value.get("data").cloned().unwrap_or(value))
            .find(|value| {
                value.get("hook_event_name").and_then(|v| v.as_str()) == Some("UserPromptSubmit")
                    || value.get("event").and_then(|v| v.as_str()) == Some("input")
            })
            .unwrap();
        payload["session_id"] = serde_json::json!("evidence-session");
        payload["tmux_pane"] = serde_json::json!("%7");
        payload.as_object_mut().unwrap().remove("pid");
        if harness == Harness::Pi {
            payload["payload"]["text"] = serde_json::json!("[m123] Read: atm inbox --id m123");
        } else {
            payload["prompt"] = serde_json::json!("[m123] Read: atm inbox --id m123");
        }
        payload
    }

    #[tokio::test]
    async fn message_evidence_tap_receives_all_three_captured_prompt_shapes() {
        use atm_core::Harness;
        for harness in [Harness::ClaudeCode, Harness::Pi, Harness::Codex] {
            let (mut handler, evidence) = evidence_handler(harness).await;
            let payload = captured_prompt(harness);
            match harness {
                Harness::ClaudeCode => handler.handle_hook_event(payload).await.unwrap(),
                Harness::Pi => handler.handle_pi_event(payload).await.unwrap(),
                Harness::Codex => handler.handle_codex_event(payload).await.unwrap(),
                _ => unreachable!(),
            }
            assert_eq!(
                evidence
                    .receipt("m123".parse().unwrap())
                    .unwrap()
                    .unwrap()
                    .session_id
                    .as_str(),
                "evidence-session"
            );
        }
    }

    #[tokio::test]
    async fn message_evidence_later_prompt_does_not_use_first_prompt_preview() {
        let harness = atm_core::Harness::Pi;
        let (mut handler, evidence) = evidence_handler(harness).await;
        let mut first = captured_prompt(harness);
        first["payload"]["text"] = serde_json::json!("earlier unrelated preview");
        handler.handle_pi_event(first).await.unwrap();
        assert!(evidence.receipt("m123".parse().unwrap()).unwrap().is_none());
        // There is deliberately no wake consumer: ingestion must complete even
        // with the service stalled, retaining the exact later receipt.
        handler
            .handle_pi_event(captured_prompt(harness))
            .await
            .unwrap();
        let row = handler
            .registry
            .get_session(SessionId::new("evidence-session"))
            .await
            .unwrap();
        assert_eq!(
            row.first_prompt.as_deref(),
            Some("earlier unrelated preview")
        );
        assert!(evidence.receipt("m123".parse().unwrap()).unwrap().is_some());
    }

    #[tokio::test]
    async fn message_evidence_lost_history_fails_closed_without_rolling_back_hook() {
        let (handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let mut handler = handler.with_message_evidence(
            Some(evidence.clone()),
            Some(crate::org::message_history::MessageHistoryHandle::disabled(
                "synthetic history loss",
            )),
        );
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert!(matches!(
            evidence.receipt("m123".parse().unwrap()),
            Err(crate::org::messages::MessageError::Unavailable(_))
        ));
        assert_eq!(
            handler
                .registry
                .get_session(SessionId::new("evidence-session"))
                .await
                .unwrap()
                .first_prompt
                .as_deref(),
            Some("[m123] Read: atm inbox --id m123")
        );
    }

    #[tokio::test]
    async fn message_evidence_apply_error_holds_the_event_session() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let other = arm_other_message(&evidence);
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let (events, _) = tokio::sync::broadcast::channel(1);
        handler.registry = RegistryHandle::new(sender, events);
        drop(receiver);
        assert!(handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .is_err());
        // The lost event was this session's exact reference: hold, never
        // confirm, and leave every other message alone.
        assert_held_without_receipt(&evidence);
        assert_untouched(&evidence, other);
    }

    #[tokio::test]
    async fn message_evidence_status_line_error_holds_the_event_session() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::ClaudeCode).await;
        let other = arm_other_message(&evidence);
        let snapshots = fake_registry(
            &mut handler,
            Err(crate::registry::RegistryError::RegistryFull { max: 100 }),
            FakeSnapshot::Rows(vec![]),
        );
        let mut data: serde_json::Value = serde_json::from_str(
            include_str!(
                "../../../atm-claude-adapter/tests/fixtures/claude_status_line_2.1.310.jsonl"
            )
            .lines()
            .next()
            .unwrap(),
        )
        .unwrap();
        data["session_id"] = serde_json::json!("evidence-session");
        assert!(handler.handle_status_update(data).await.is_err());
        assert_held_without_receipt(&evidence);
        assert_untouched(&evidence, other);
        // A failed apply needs no snapshot to be unproven.
        assert_eq!(snapshots.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn message_evidence_codex_derived_metadata_has_no_second_tap() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Codex).await;
        let row = handler
            .registry
            .get_session(SessionId::new("evidence-session"))
            .await
            .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let (events, _) = tokio::sync::broadcast::channel(8);
        handler.registry = RegistryHandle::new(sender, events);
        let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_by_actor = observed.clone();
        let actor = tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    crate::registry::RegistryCommand::ApplyLifecycleEvent {
                        respond_to, ..
                    } => {
                        let _ = respond_to.send(Ok(()));
                    }
                    crate::registry::RegistryCommand::GetAllSessions { respond_to } => {
                        observed_by_actor.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = respond_to.send(vec![row.clone()]);
                    }
                    _ => panic!("unexpected registry operation"),
                }
            }
        });
        let mut payload = captured_prompt(atm_core::Harness::Codex);
        payload["model"] = serde_json::json!("synthetic-model");
        payload.as_object_mut().unwrap().remove("transcript_path");
        handler.handle_codex_event(payload).await.unwrap();
        assert!(evidence.receipt("m123".parse().unwrap()).unwrap().is_some());
        // A second tap on synthesized model metadata would request another
        // eligibility observation and fabricate another genuine event clock.
        assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(handler);
        actor.await.unwrap();
    }

    #[tokio::test]
    async fn message_evidence_child_context_never_confirms_parent() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::ClaudeCode).await;
        let mut payload = captured_prompt(atm_core::Harness::ClaudeCode);
        payload["agent_id"] = serde_json::json!("child-agent");
        payload["agent_type"] = serde_json::json!("general-purpose");
        handler.handle_hook_event(payload).await.unwrap();
        let facts = evidence.snapshot("m123".parse().unwrap()).unwrap();
        assert!(facts.receipt.is_none());
        assert!(!facts.probably_landed);
    }

    #[tokio::test]
    async fn message_evidence_ignored_event_holds_but_never_confirms() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let other = arm_other_message(&evidence);
        handler
            .registry
            .remove(SessionId::new("evidence-session"))
            .await
            .unwrap();
        // The registry answers Ok and ignores it: no pid, no row.
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert!(handler
            .registry
            .get_session(SessionId::new("evidence-session"))
            .await
            .is_none());
        assert_held_without_receipt(&evidence);
        assert_untouched(&evidence, other);
    }

    #[tokio::test]
    async fn message_evidence_wrong_original_pane_does_not_borrow_row_pane() {
        // The attempted session's hook names another pane than its row and its
        // scope (%7). The tap never repairs the event from the row, so it
        // cannot confirm; it is still that session's reference, so it holds.
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let other = arm_other_message(&evidence);
        let mut payload = captured_prompt(atm_core::Harness::Pi);
        payload["tmux_pane"] = serde_json::json!("%8");
        handler.handle_pi_event(payload).await.unwrap();
        assert_held_without_receipt(&evidence);
        // m200 is armed on %8 for another session: the pane alone is no claim.
        assert_untouched(&evidence, other);
    }

    #[tokio::test]
    async fn message_evidence_attempted_session_activity_on_another_pane_holds() {
        // Synthetic: the shape atm-codex-adapter's crate docs record for
        // codex-cli 0.163.0 ("Known gaps"): a second TUI's hooks carry the
        // pane of the TUI that started the shared daemon. The session runs in
        // %7 and every hook of it says %8.
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Codex).await;
        let mut payload = captured_prompt(atm_core::Harness::Codex);
        payload["tmux_pane"] = serde_json::json!("%8");
        payload["prompt"] = serde_json::json!("an ordinary prompt");
        handler.handle_codex_event(payload).await.unwrap();
        assert_held_without_receipt(&evidence);
    }

    #[tokio::test]
    async fn message_evidence_session_end_is_activity_after_parent_removal() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let mut payload = captured_prompt(atm_core::Harness::Pi);
        payload["event"] = serde_json::json!("session_shutdown");
        payload["payload"] = serde_json::json!({"type":"session_shutdown"});
        handler.handle_pi_event(payload).await.unwrap();
        assert!(handler
            .registry
            .get_session(SessionId::new("evidence-session"))
            .await
            .is_none());
        assert!(
            evidence
                .snapshot("m123".parse().unwrap())
                .unwrap()
                .probably_landed
        );
        assert!(evidence.receipt("m123".parse().unwrap()).unwrap().is_none());
    }

    #[tokio::test]
    async fn message_evidence_accepted_status_is_activity_not_receipt() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::ClaudeCode).await;
        let mut data: serde_json::Value = serde_json::from_str(
            include_str!(
                "../../../atm-claude-adapter/tests/fixtures/claude_status_line_2.1.310.jsonl"
            )
            .lines()
            .next()
            .unwrap(),
        )
        .unwrap();
        data["session_id"] = serde_json::json!("evidence-session");
        handler.handle_status_update(data).await.unwrap();
        let facts = evidence.snapshot("m123".parse().unwrap()).unwrap();
        assert!(facts.probably_landed);
        assert!(facts.receipt.is_none());
    }

    /// What the fake registry answers to the tap's one snapshot request.
    enum FakeSnapshot {
        Rows(Vec<atm_core::SessionView>),
        /// The actor stays up but drops the reply, as a dying actor does.
        DropReply,
        /// A stalled registry: the reply is held until the handler is gone.
        Hold,
    }

    /// Replaces the handler's registry with a scripted actor. Returns the
    /// number of snapshots it was asked for.
    fn fake_registry(
        handler: &mut EvidenceHandler,
        apply: Result<(), crate::registry::RegistryError>,
        snapshot: FakeSnapshot,
    ) -> Arc<std::sync::atomic::AtomicUsize> {
        use crate::registry::RegistryCommand;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let (events, _) = tokio::sync::broadcast::channel(8);
        handler.registry = RegistryHandle::new(sender, events);
        let snapshots = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = snapshots.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(command) = receiver.recv().await {
                match command {
                    RegistryCommand::ApplyLifecycleEvent { respond_to, .. }
                    | RegistryCommand::UpdateFromStatusLine { respond_to, .. } => {
                        let _ = respond_to.send(apply.clone());
                    }
                    RegistryCommand::GetAllSessions { respond_to } => {
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        match &snapshot {
                            FakeSnapshot::Rows(rows) => {
                                let _ = respond_to.send(rows.clone());
                            }
                            FakeSnapshot::DropReply => drop(respond_to),
                            FakeSnapshot::Hold => held.push(respond_to),
                        }
                    }
                    _ => panic!("unexpected registry operation"),
                }
            }
        });
        snapshots
    }

    #[tokio::test]
    async fn message_evidence_disabled_takes_no_registry_snapshot() {
        let (handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let disabled = crate::org::message_evidence::MessageEvidence::disabled(
            evidence.clock().clone(),
            "synthetic broken invariant",
        );
        let mut handler = handler.with_message_evidence(Some(disabled), None);
        let snapshots = fake_registry(&mut handler, Ok(()), FakeSnapshot::Rows(vec![]));
        for _ in 0..3 {
            handler
                .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
                .await
                .unwrap();
        }
        // The hook is still applied; a disabled evidence costs it nothing more.
        assert_eq!(snapshots.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    fn m123() -> atm_org::MessageId {
        "m123".parse().unwrap()
    }

    /// A second armed message, for another session on another pane.
    fn arm_other_message(
        evidence: &crate::org::message_evidence::MessageEvidence,
    ) -> atm_org::MessageId {
        let id = "m200".parse().unwrap();
        evidence
            .register(id, "[m200] Read: atm inbox --id m200".into())
            .unwrap();
        evidence
            .arm(crate::org::message_evidence::AttemptScope {
                message_id: id,
                attempt_seq: 201,
                recipient: "demo/other".parse().unwrap(),
                pane: crate::control::reservation::PaneId::parse("%8").unwrap(),
                harness: atm_core::Harness::Pi,
                session_id: Some(SessionId::new("other-session")),
                excluded_sessions: Default::default(),
            })
            .unwrap();
        id
    }

    /// The outcome of an unproven event for m123: held, and evidence still on.
    fn assert_held_without_receipt(evidence: &crate::org::message_evidence::MessageEvidence) {
        let facts = evidence.snapshot(m123()).unwrap();
        assert!(facts.receipt.is_none());
        assert!(facts.probably_landed);
    }

    fn assert_untouched(
        evidence: &crate::org::message_evidence::MessageEvidence,
        id: atm_org::MessageId,
    ) {
        let facts = evidence.snapshot(id).unwrap();
        assert!(facts.receipt.is_none());
        assert!(!facts.probably_landed);
    }

    async fn evidence_row(handler: &EvidenceHandler) -> atm_core::SessionView {
        handler
            .registry
            .get_session(SessionId::new("evidence-session"))
            .await
            .unwrap()
    }

    /// Replaces the registered row with a changed copy, in the real registry.
    async fn replace_evidence_row(
        handler: &EvidenceHandler,
        change: impl FnOnce(&mut SessionDomain),
    ) {
        let id = SessionId::new("evidence-session");
        handler.registry.remove(id.clone()).await.unwrap();
        let mut session = SessionDomain::new(id, AgentType::GeneralPurpose, Model::Unknown);
        session.harness = atm_core::Harness::Pi;
        session.tmux_pane = Some("%7".into());
        change(&mut session);
        handler.registry.register(session).await.unwrap();
    }

    // No tracing subscriber is installed here, so a reaction that lives in a
    // log macro's arguments would not run.
    #[tokio::test]
    async fn message_evidence_registry_reply_dropped_after_apply_holds() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let other = arm_other_message(&evidence);
        let snapshots = fake_registry(&mut handler, Ok(()), FakeSnapshot::DropReply);
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_eq!(snapshots.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(handler.registry.is_connected());
        assert_held_without_receipt(&evidence);
        assert_untouched(&evidence, other);
    }

    #[tokio::test(start_paused = true)]
    async fn message_evidence_registry_snapshot_stall_holds() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let other = arm_other_message(&evidence);
        let snapshots = fake_registry(&mut handler, Ok(()), FakeSnapshot::Hold);
        // The paused clock runs the snapshot's bound out; nothing waits for real.
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_eq!(snapshots.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_held_without_receipt(&evidence);
        assert_untouched(&evidence, other);
    }

    #[tokio::test]
    async fn message_evidence_failed_apply_is_never_proven() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        // The snapshot would prove the parent, but the event was not applied.
        let row = evidence_row(&handler).await;
        let snapshots = fake_registry(
            &mut handler,
            Err(crate::registry::RegistryError::RegistryFull { max: 100 }),
            FakeSnapshot::Rows(vec![row]),
        );
        assert!(handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .is_err());
        assert_held_without_receipt(&evidence);
        assert_eq!(snapshots.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn message_evidence_pane_less_event_takes_no_snapshot() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let row = evidence_row(&handler).await;
        let snapshots = fake_registry(&mut handler, Ok(()), FakeSnapshot::Rows(vec![row]));
        let mut payload = captured_prompt(atm_core::Harness::Pi);
        payload.as_object_mut().unwrap().remove("tmux_pane");
        handler.handle_pi_event(payload).await.unwrap();
        // Its session is the bound one, so it is activity; it cannot confirm.
        assert_held_without_receipt(&evidence);
        assert_eq!(snapshots.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn message_evidence_activity_only_events_take_no_snapshot() {
        // A status line and a SessionEnd can neither bind nor confirm.
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let snapshots = fake_registry(&mut handler, Ok(()), FakeSnapshot::Rows(vec![]));
        let mut end = captured_prompt(atm_core::Harness::Pi);
        end["event"] = serde_json::json!("session_shutdown");
        end["payload"] = serde_json::json!({"type":"session_shutdown"});
        handler.handle_pi_event(end).await.unwrap();
        assert_held_without_receipt(&evidence);

        let (mut handler, evidence) = evidence_handler(atm_core::Harness::ClaudeCode).await;
        let status_snapshots = fake_registry(&mut handler, Ok(()), FakeSnapshot::Rows(vec![]));
        let mut data: serde_json::Value = serde_json::from_str(
            include_str!(
                "../../../atm-claude-adapter/tests/fixtures/claude_status_line_2.1.310.jsonl"
            )
            .lines()
            .next()
            .unwrap(),
        )
        .unwrap();
        data["session_id"] = serde_json::json!("evidence-session");
        handler.handle_status_update(data).await.unwrap();
        assert_held_without_receipt(&evidence);
        for count in [snapshots, status_snapshots] {
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn message_evidence_shared_pane_holds_never_drops() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        // Another parent row on the same pane: a nested run, or a previous
        // occupant not yet cleaned up.
        let mut nested = SessionDomain::new(
            SessionId::new("nested-session"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        nested.harness = atm_core::Harness::Pi;
        nested.tmux_pane = Some("%7".into());
        handler.registry.register(nested).await.unwrap();
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_held_without_receipt(&evidence);
    }

    #[tokio::test]
    async fn message_evidence_duplicate_session_rows_hold() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let row = evidence_row(&handler).await;
        fake_registry(
            &mut handler,
            Ok(()),
            FakeSnapshot::Rows(vec![row.clone(), row]),
        );
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_held_without_receipt(&evidence);
    }

    #[tokio::test]
    async fn message_evidence_child_row_never_confirms() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        replace_evidence_row(&handler, |row| {
            row.parent_session_id = Some(SessionId::new("lead-session"));
        })
        .await;
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_held_without_receipt(&evidence);
    }

    #[tokio::test]
    async fn message_evidence_row_of_another_harness_never_confirms() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        replace_evidence_row(&handler, |row| row.harness = atm_core::Harness::Codex).await;
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_held_without_receipt(&evidence);
    }

    #[tokio::test]
    async fn message_evidence_row_on_another_pane_never_confirms() {
        // The event names the scope's pane, but the registry knows the
        // session on another one: the row does not prove the event.
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        replace_evidence_row(&handler, |row| row.tmux_pane = Some("%8".into())).await;
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_held_without_receipt(&evidence);
    }

    #[tokio::test]
    async fn message_evidence_unparsable_pane_is_an_unknown_pane() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let mut payload = captured_prompt(atm_core::Harness::Pi);
        payload["tmux_pane"] = serde_json::json!("7");
        handler.handle_pi_event(payload).await.unwrap();
        assert_held_without_receipt(&evidence);
    }

    #[tokio::test]
    async fn message_evidence_registry_full_from_unrelated_process_leaves_evidence_on() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        for n in 1..crate::registry::MAX_SESSIONS {
            handler
                .registry
                .register(SessionDomain::new(
                    SessionId::new(format!("filler-{n}")),
                    AgentType::GeneralPurpose,
                    Model::Unknown,
                ))
                .await
                .unwrap();
        }
        // An unrelated new process on another pane; no such pid exists.
        let mut payload = captured_prompt(atm_core::Harness::Pi);
        payload["session_id"] = serde_json::json!("unrelated-session");
        payload["tmux_pane"] = serde_json::json!("%9");
        payload["pid"] = serde_json::json!(4_000_000_000_u32);
        payload["payload"]["text"] = serde_json::json!("an unrelated prompt");
        assert!(matches!(
            handler.handle_pi_event(payload).await,
            Err(ConnectionError::RegistryError(_))
        ));
        assert_untouched(&evidence, m123());
        // The armed session's own receipt still confirms afterwards.
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert!(evidence.receipt(m123()).unwrap().is_some());
    }

    #[tokio::test]
    async fn message_evidence_old_session_with_occupant_pid_holds_but_never_confirms() {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        // The occupant's row, keyed by a pid no process has.
        let pid = 4_000_000_000_u32;
        let occupant = SessionId::new("evidence-session");
        handler.registry.remove(occupant.clone()).await.unwrap();
        handler
            .registry
            .register_discovered(
                occupant,
                pid,
                "/".into(),
                Some("%7".into()),
                atm_core::Harness::Pi,
            )
            .await
            .unwrap();
        let mut old = captured_prompt(atm_core::Harness::Pi);
        old["session_id"] = serde_json::json!("old-session");
        old["pid"] = serde_json::json!(pid);
        handler.handle_pi_event(old).await.unwrap();
        // The registry routed the old session's prompt into the occupant's row...
        assert_eq!(
            evidence_row(&handler).await.first_prompt.as_deref(),
            Some("[m123] Read: atm inbox --id m123")
        );
        // ...and the tap credited the occupant with no receipt. The reference
        // itself was submitted by someone, so its message is held.
        assert_held_without_receipt(&evidence);
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_eq!(
            evidence
                .receipt(m123())
                .unwrap()
                .unwrap()
                .session_id
                .as_str(),
            "evidence-session"
        );
    }

    fn request_router() -> Arc<super::super::methods::MethodRouter> {
        let mut router = super::super::methods::MethodRouter::default();
        router.register("wait", Duration::from_secs(1000), |ctx, params| async move {
            tokio::time::sleep(Duration::from_secs(params.as_u64().unwrap_or(0))).await;
            Ok(serde_json::json!({"client":ctx.client_id.0,"pane":ctx.from_pane.map(|p|p.0)}))
        }).unwrap();
        Arc::new(router)
    }

    async fn request_connection(
        router: Arc<super::super::methods::MethodRouter>,
    ) -> (
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        CancellationToken,
        tokio::task::JoinHandle<Option<String>>,
    ) {
        let (ours, peer) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(ours);
        let cancel = CancellationToken::new();
        let handler = ConnectionHandler::new(
            read,
            write,
            crate::registry::spawn_registry(),
            Arc::new(RwLock::new(HashMap::new())),
            1,
            cancel.clone(),
            router,
        );
        let run = tokio::spawn(handler.run());
        let (read, mut write) = tokio::io::split(peer);
        let mut read = BufReader::new(read);
        send_client(&mut write, ClientMessage::connect(Some("caller".into()))).await;
        assert!(matches!(
            receive(&mut read).await,
            DaemonMessage::Connected { .. }
        ));
        (read, write, cancel, run)
    }

    async fn send_client<W: AsyncWrite + Unpin>(writer: &mut W, msg: ClientMessage) {
        timeout(
            Duration::from_secs(60),
            writer.write_all(format!("{}\n", serde_json::to_string(&msg).unwrap()).as_bytes()),
        )
        .await
        .unwrap()
        .unwrap();
    }

    async fn receive<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> DaemonMessage {
        let mut line = String::new();
        timeout(Duration::from_secs(1200), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn request(id: &str, seconds: u64) -> ClientMessage {
        ClientMessage::request(
            id.into(),
            "wait".into(),
            serde_json::json!(seconds),
            Some("%12".into()),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn requests_complete_out_of_order_while_ping_is_served() {
        let (mut read, mut write, cancel, run) = request_connection(request_router()).await;
        send_client(&mut write, request("slow", 10)).await;
        send_client(&mut write, request("fast", 0)).await;
        send_client(&mut write, ClientMessage::ping(99)).await;
        let mut first = Vec::new();
        for _ in 0..2 {
            match receive(&mut read).await {
                DaemonMessage::Pong { seq: 99 } => first.push("ping".to_owned()),
                DaemonMessage::Response {
                    id,
                    payload: atm_protocol::ResponsePayload::Result(value),
                } => {
                    assert_eq!(value, serde_json::json!({"client":"caller","pane":"%12"}));
                    first.push(id);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(first.contains(&"fast".into()));
        assert!(first.contains(&"ping".into()));
        assert!(
            matches!(receive(&mut read).await, DaemonMessage::Response { id, .. } if id == "slow")
        );
        cancel.cancel();
        run.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn completing_request_does_not_drop_partial_next_line() {
        let (mut read, mut write, cancel, run) = request_connection(request_router()).await;
        send_client(&mut write, request("first", 1)).await;
        let next = serde_json::to_vec(&request("second", 0)).unwrap();
        let (prefix, suffix) = next.split_at(next.len() / 2);
        write.write_all(prefix).await.unwrap();
        assert!(
            matches!(receive(&mut read).await, DaemonMessage::Response { id, .. } if id == "first")
        );
        write.write_all(suffix).await.unwrap();
        write.write_all(b"\n").await.unwrap();
        assert!(
            matches!(receive(&mut read).await, DaemonMessage::Response { id, .. } if id == "second")
        );
        cancel.cancel();
        run.await.unwrap();
    }

    #[test]
    fn watch_reader_timeout_has_timeout_code_without_id() {
        let frame =
            serde_json::to_value(watch_error(crate::org::OrgServiceError::Timeout)).unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "timeout");
        assert!(frame.get("id").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn watch_only_survives_idle_timeout_then_unwatch_restores_it() {
        let (ours, peer) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(ours);
        let cancel = CancellationToken::new();
        let mut handler = ConnectionHandler::new(
            read,
            write,
            crate::registry::spawn_registry(),
            Arc::new(RwLock::new(HashMap::new())),
            1,
            cancel,
            request_router(),
        );
        // Idle transport only: a fake quiet watch avoids paused clocks racing
        // filesystem workers. Real watch setup/delivery is tested over sockets.
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let mut task = JoinSet::new();
        task.spawn(async move {
            stopped.cancelled().await;
        });
        handler.watcher = Some(Forwarder { stop, task });
        let run = tokio::spawn(handler.run());
        let (read, mut write) = tokio::io::split(peer);
        let mut read = BufReader::new(read);
        send_client(&mut write, ClientMessage::connect(None)).await;
        assert!(matches!(
            receive(&mut read).await,
            DaemonMessage::Connected { .. }
        ));
        tokio::time::advance(READ_TIMEOUT + Duration::from_secs(1)).await;
        send_client(&mut write, ClientMessage::ping(1)).await;
        assert!(matches!(
            receive(&mut read).await,
            DaemonMessage::Pong { seq: 1 }
        ));
        send_client(&mut write, ClientMessage::unwatch()).await;
        send_client(&mut write, ClientMessage::ping(2)).await;
        assert!(matches!(
            receive(&mut read).await,
            DaemonMessage::Pong { seq: 2 }
        ));
        tokio::time::advance(READ_TIMEOUT - Duration::from_secs(1)).await;
        send_client(&mut write, ClientMessage::ping(3)).await;
        assert!(matches!(
            receive(&mut read).await,
            DaemonMessage::Pong { seq: 3 }
        ));
        tokio::time::advance(READ_TIMEOUT).await;
        let mut line = String::new();
        assert_eq!(
            timeout(Duration::from_secs(60), read.read_line(&mut line))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        run.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn pending_request_exempts_idle_then_completion_restarts_idle() {
        let (mut read, mut write, _cancel, run) = request_connection(request_router()).await;
        send_client(&mut write, request("long", 301)).await;
        assert!(
            matches!(receive(&mut read).await, DaemonMessage::Response { id, .. } if id == "long")
        );
        let start = tokio::time::Instant::now();
        let mut line = String::new();
        assert_eq!(
            timeout(Duration::from_secs(310), read.read_line(&mut line))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(start.elapsed() >= READ_TIMEOUT);
        run.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn tail_response_limit_also_applies_when_request_queue_is_full() {
        let (mut read, mut write, cancel, run) = request_connection(request_router()).await;
        for n in 0..64 {
            send_client(&mut write, request(&n.to_string(), 10)).await;
        }
        send_client(&mut write, ClientMessage::ping(42)).await;
        assert!(matches!(
            receive(&mut read).await,
            DaemonMessage::Pong { seq: 42 }
        ));
        let mut message = ClientMessage::request(
            String::new(),
            "log.tail".into(),
            serde_json::json!({}),
            None,
        );
        let base = serde_json::to_vec(&message).unwrap().len() + 1;
        if let MessageType::Request { id, .. } = &mut message.message {
            *id = "i".repeat(MAX_MESSAGE_SIZE - base);
        }
        send_client(&mut write, message).await;
        let mut bytes = Vec::new();
        assert_eq!(
            timeout(Duration::from_secs(60), read.read_to_end(&mut bytes))
                .await
                .unwrap()
                .unwrap(),
            0,
            "oversized capacity error started a frame"
        );
        cancel.cancel();
        timeout(Duration::from_secs(60), run)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn request_capacity_is_bounded_and_completed_slots_are_reused() {
        let (mut read, mut write, cancel, run) = request_connection(request_router()).await;
        for n in 0..64 {
            send_client(&mut write, request(&n.to_string(), 10)).await;
        }
        send_client(&mut write, request("overflow", 10)).await;
        assert!(
            matches!(receive(&mut read).await, DaemonMessage::Response { id, payload:atm_protocol::ResponsePayload::Error(error) } if id == "overflow" && error.code == "unavailable")
        );
        for _ in 0..64 {
            assert!(matches!(
                receive(&mut read).await,
                DaemonMessage::Response {
                    payload: atm_protocol::ResponsePayload::Result(_),
                    ..
                }
            ));
        }
        send_client(&mut write, request("reused", 0)).await;
        assert!(
            matches!(receive(&mut read).await, DaemonMessage::Response { id, payload:atm_protocol::ResponsePayload::Result(_) } if id == "reused")
        );
        cancel.cancel();
        run.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_requests_even_while_draining() {
        for half_close in [true, false] {
            let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
            let (dropped_tx, mut dropped_rx) = tokio::sync::mpsc::unbounded_channel();
            struct Guard(tokio::sync::mpsc::UnboundedSender<()>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    let _ = self.0.send(());
                }
            }
            let mut router = super::super::methods::MethodRouter::default();
            router
                .register("wait", Duration::from_secs(1000), move |_, _| {
                    let started = started_tx.clone();
                    let dropped = dropped_tx.clone();
                    async move {
                        let _guard = Guard(dropped);
                        started.send(()).unwrap();
                        std::future::pending().await
                    }
                })
                .unwrap();
            let (_read, mut write, cancel, run) = request_connection(Arc::new(router)).await;
            send_client(&mut write, request("pending", 0)).await;
            timeout(Duration::from_secs(5), started_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if half_close {
                write.shutdown().await.unwrap();
            }
            cancel.cancel();
            timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
            timeout(Duration::from_secs(5), dropped_rx.recv())
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn shutdown_interrupts_blocked_replies_including_disconnect_drain() {
        for disconnect in [false, true] {
            let release = Arc::new(tokio::sync::Notify::new());
            let ready = release.clone();
            let mut router = MethodRouter::default();
            router
                .register("wait", Duration::from_secs(30), move |_, _| {
                    let ready = ready.clone();
                    async move {
                        ready.notified().await;
                        Ok(serde_json::json!("x".repeat(4 * 1024 * 1024)))
                    }
                })
                .unwrap();
            let (ours, peer) = tokio::io::duplex(4096);
            let (read, write) = tokio::io::split(ours);
            let cancel = CancellationToken::new();
            let handler = ConnectionHandler::new(
                read,
                write,
                crate::registry::spawn_registry(),
                Arc::new(RwLock::new(HashMap::new())),
                1,
                cancel.clone(),
                Arc::new(router),
            );
            let mut run = Box::pin(handler.run());
            let (read, mut write) = tokio::io::split(peer);
            let mut read = BufReader::new(read);
            let probe = async {
                send_client(&mut write, ClientMessage::connect(None)).await;
                assert!(matches!(
                    receive(&mut read).await,
                    DaemonMessage::Connected { .. }
                ));
                send_client(&mut write, request("blocked", 0)).await;
                if disconnect {
                    send_client(&mut write, ClientMessage::disconnect()).await;
                }
                release.notify_one();
                // The first byte proves the reply write started. Keep the rest
                // unread; cancellation must complete without polling the peer.
                read.read_u8().await.unwrap();
            };
            tokio::select! {
                _ = &mut run => panic!("handler exited before its reply"),
                result = timeout(Duration::from_secs(60), probe) => result.unwrap(),
            }
            cancel.cancel();
            tokio::select! {
                biased;
                _ = &mut run => {},
                _ = std::future::ready(()) => panic!("shutdown waited for a blocked reply (disconnect={disconnect})"),
            }
        }
    }

    #[tokio::test]
    async fn shutdown_interrupts_blocked_subscription_forwarder() {
        let (ours, peer) = UnixStream::pair().unwrap();
        let (read, write) = ours.into_split();
        let cancel = CancellationToken::new();
        let mut handler = ConnectionHandler::new(
            read,
            write,
            crate::registry::spawn_registry(),
            Arc::new(RwLock::new(HashMap::new())),
            1,
            cancel.clone(),
            request_router(),
        );
        let (tx, rx) = broadcast::channel(1);
        handler.forwarder = Some(Forwarder::spawn(
            "stuck".into(),
            rx,
            handler.writer_handle(),
            None,
            &handler.peer_failed,
        ));
        let mut event = updated("stuck");
        if let SessionEvent::Updated { session } = &mut event {
            session.first_prompt = Some("x".repeat(4 * 1024 * 1024));
        }
        tx.send(event).unwrap();
        let mut peer = BufReader::new(peer);
        timeout(Duration::from_secs(60), peer.read_u8())
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
        tokio::select! {
            biased;
            result = handler.process_messages() => result.unwrap(),
            _ = std::future::ready(()) => panic!("shutdown waited for a blocked forwarder"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn half_close_and_disconnect_drain_accepted_replies() {
        for half_close in [true, false] {
            let (mut read, mut write, _cancel, run) = request_connection(request_router()).await;
            send_client(&mut write, request("accepted", 1)).await;
            if half_close {
                write.shutdown().await.unwrap();
            } else {
                send_client(&mut write, ClientMessage::disconnect()).await;
            }
            assert!(
                matches!(receive(&mut read).await, DaemonMessage::Response { id, .. } if id == "accepted")
            );
            let mut line = String::new();
            assert_eq!(
                timeout(Duration::from_secs(5), read.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            run.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_reply_finishes_remaining_handlers_without_more_writes() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = finished.clone();
        let mut router = super::super::methods::MethodRouter::default();
        router
            .register("wait", Duration::from_secs(100), move |_, params| {
                let completed = completed.clone();
                async move {
                    if params == serde_json::json!(20) {
                        tokio::time::sleep(Duration::from_secs(20)).await;
                        completed.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(serde_json::json!("must not write after failed reply"))
                    } else {
                        Ok(serde_json::json!("x".repeat(2_000_000)))
                    }
                }
            })
            .unwrap();
        let (mut read, mut write, _cancel, run) = request_connection(Arc::new(router)).await;
        send_client(&mut write, request("blocked", 0)).await;
        send_client(&mut write, request("mutation", 20)).await;
        write.shutdown().await.unwrap();
        timeout(Duration::from_secs(40), run)
            .await
            .unwrap()
            .unwrap();
        assert!(
            finished.load(std::sync::atomic::Ordering::SeqCst),
            "accepted handler was aborted"
        );
        let mut bytes = Vec::new();
        read.read_to_end(&mut bytes).await.unwrap();
        assert!(
            !bytes.contains(&b'\n'),
            "a second reply followed a partial line"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn method_timeout_is_a_correlated_error_and_connection_survives() {
        let mut router = super::super::methods::MethodRouter::default();
        router
            .register("wait", Duration::from_millis(5), |_, _| {
                std::future::pending()
            })
            .unwrap();
        let (mut read, mut write, cancel, run) = request_connection(Arc::new(router)).await;
        send_client(&mut write, request("timed", 0)).await;
        assert!(
            matches!(receive(&mut read).await, DaemonMessage::Response { id, payload:atm_protocol::ResponsePayload::Error(error) } if id == "timed" && error.code == "timeout")
        );
        send_client(&mut write, ClientMessage::ping(7)).await;
        assert!(matches!(
            receive(&mut read).await,
            DaemonMessage::Pong { seq: 7 }
        ));
        cancel.cancel();
        run.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_request_reply_closes_connection() {
        let mut router = super::super::methods::MethodRouter::default();
        router
            .register("wait", Duration::from_secs(1), |_, _| async {
                Ok(serde_json::Value::String("x".repeat(2_000_000)))
            })
            .unwrap();
        let (mut read, mut write, _cancel, run) = request_connection(Arc::new(router)).await;
        send_client(&mut write, request("large", 0)).await;
        timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let mut bytes = Vec::new();
        timeout(Duration::from_secs(5), read.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !bytes.ends_with(b"\n"),
            "large response should have been interrupted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reply_deadline_includes_waiting_for_the_writer_lock() {
        let (writer, _peer) = subscriber_pair();
        let _guard = writer.lock().await;
        let result = timeout(
            Duration::from_secs(20),
            write_message(&writer, &DaemonMessage::pong(1)),
        )
        .await;
        assert!(matches!(result, Ok(Err(ConnectionError::WriteTimeout))));
    }

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

    #[tokio::test]
    async fn reply_writer_frames_one_line_and_reports_closed_peer() {
        let (writer, peer) = subscriber_pair();
        write_message(&writer, &DaemonMessage::pong(42))
            .await
            .unwrap();
        let mut reader = BufReader::new(peer);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "{\"type\":\"pong\",\"seq\":42}\n");
        drop(reader);
        assert!(matches!(
            write_message(&writer, &DaemonMessage::pong(43)).await,
            Err(ConnectionError::Io(_))
        ));
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
