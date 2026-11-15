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
pub mod methods;
pub mod stop;

pub use connection::{
    parse_status_line, ConnectionError, ConnectionHandler, Subscriber, SubscriberWriter,
    SubscribersMap,
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

/// Maximum number of concurrent TUI clients (subscribed connections).
/// A workspace runs one `atm --compact` sidebar per window, and each
/// sidebar is a subscriber.
pub const MAX_TUI_CLIENTS: usize = 64;

/// How long shutdown waits for connection handlers before aborting them
const CONNECTION_DRAIN_TIMEOUT: Duration = stop::CONNECTIONS;

/// How long the accept loop pauses after a failed accept
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Unix socket server for the ATM daemon.
///
/// Manages client connections and event broadcasting.
pub struct DaemonServer {
    /// Path to the Unix socket
    socket_path: PathBuf,

    /// Handle to the session registry
    registry: RegistryHandle,

    /// Cloneable method dependencies, injectable without changing callers.
    method_services: methods::MethodServices,

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
        Self::with_services(
            socket_path,
            methods::MethodServices::new(registry),
            cancel_token,
        )
    }

    /// Creates a server with explicitly supplied services (including test fakes).
    pub fn with_services(
        socket_path: impl Into<PathBuf>,
        services: methods::MethodServices,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            socket_path: socket_path.into(),
            registry: services.registry.clone(),
            method_services: services,
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
        let mut services = self.method_services.clone();
        // Control drains only after connections; parent cancellation must not
        // interrupt a paste before its Enter or discard its pending audit.
        let control_cancel = CancellationToken::new();
        let control_task = if services.control.is_none() {
            let (control, task) = crate::control::service::spawn_control(
                Box::new(atm_tmux::RealTmuxClient::new()),
                self.registry.clone(),
                services.org.clone(),
                control_cancel.clone(),
            );
            services.control = Some(control);
            Some(task)
        } else {
            None
        };
        // Only a control this server started is this server's to stop.
        let own_control = control_task.as_ref().and(services.control.clone());
        let agents_cancel = CancellationToken::new();
        let agents_task = if services.agents.is_none() && services.org.status().is_ok() {
            services.control.clone().map(|control| {
                let (agents, task) = crate::org::agents::spawn_agents(
                    services.org.clone(),
                    self.registry.clone(),
                    control,
                    agents_cancel.clone(),
                );
                // One handover service for the method and the watcher: the
                // note at the context line and the handovers the daemon runs
                // itself, for the agents this server runs. The watcher stops
                // with their token.
                let handovers = crate::org::agent_handover::Handovers::for_agents(
                    agents.clone(),
                    services.org.clone(),
                    self.registry.clone(),
                );
                services.handovers = Some(handovers.clone());
                crate::org::agent_auto_handover::spawn_handover_watch(
                    handovers,
                    agents_cancel.clone(),
                );
                services.agents = Some(agents);
                task
            })
        } else {
            None
        };
        // After control exists and before the first connection is accepted.
        let messages_cancel = CancellationToken::new();
        let messages_task = start_messages(&mut services, &messages_cancel);
        let decisions_cancel = CancellationToken::new();
        let decisions_task = if services.decisions.is_none() {
            let clock = crate::org::messages::MessageClock::new(
                tokio::time::Instant::now(),
                chrono::Utc::now(),
            );
            let (decisions, task) = crate::org::decisions::spawn_decisions(
                services.org.clone(),
                clock,
                decisions_cancel.clone(),
            );
            services.decisions = Some(decisions);
            Some(task)
        } else {
            None
        };
        let result = self.run_with_services(services).await;
        decisions_cancel.cancel();
        // Reserved work stops at once and its cleanups run while control and
        // the organisation writer still exist; then each stage, in order and
        // by its bound (see `stop`), which logs a stage that overruns.
        // Messages stop first: an attempt in flight settles while agents,
        // control and the organisation writer are still there for it.
        let stages = stop::Stages {
            messages: stop::Stage {
                cancel: messages_cancel,
                task: messages_task,
            },
            agents: stop::Stage {
                cancel: agents_cancel,
                task: agents_task,
            },
            control: stop::Stage {
                cancel: control_cancel,
                task: control_task,
            },
        };
        let stop_work = || {
            if let Some(control) = &own_control {
                control.stop_reserved_work();
            }
        };
        // Drain the non-input bridge alongside the existing stages, not in
        // addition to their global shutdown budget. Accepted org work survives.
        let drain_decisions = async {
            if let Some(mut task) = decisions_task {
                if timeout(stop::MESSAGES, &mut task).await.is_err() {
                    warn!("Decision bridge did not stop by its bound; aborting it");
                    task.abort();
                }
            }
        };
        tokio::join!(drain_decisions, stop::stop_stages(stop_work, stages));
        result
    }

    async fn run_with_services(
        &self,
        services: methods::MethodServices,
    ) -> Result<(), ServerError> {
        let methods = Arc::new(methods::router(services.clone())?);
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
                            self.handle_connection(&mut connections, stream, conn_num, Arc::clone(&methods), &services);
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to accept connection");
                            // An error such as EMFILE leaves the connection
                            // queued, so the next accept would fail at once.
                            // Pause instead of spinning; shutdown still wins.
                            tokio::select! {
                                _ = self.cancel_token.cancelled() => {}
                                _ = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                            }
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
        methods: Arc<methods::MethodRouter>,
        services: &methods::MethodServices,
    ) {
        let (reader, writer) = stream.into_split();
        let registry = self.registry.clone();
        let subscribers = Arc::clone(&self.subscribers);
        let cancel = self.cancel_token.child_token();
        let raw = services.raw.clone();
        let org = services.org.clone();
        let (evidence, history) = services.message_tap.clone().unzip();

        connections.spawn(async move {
            let handler = ConnectionHandler::new(
                reader,
                writer,
                registry,
                Arc::clone(&subscribers),
                connection_number,
                cancel,
                methods,
            )
            .with_raw(raw)
            .with_org(org)
            .with_message_evidence(evidence, history);
            let handler = match handler.with_disconnect_monitor() {
                Ok(handler) => handler,
                Err(error) => {
                    warn!(%error, "Cannot monitor client disconnect; rejecting connection");
                    return;
                }
            };

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

/// Start the confirmed-message service and install its tap, unless a handle
/// was injected (that one stays authoritative) or no delivery policy was
/// configured (the methods then stay explicitly unavailable). An invalid
/// policy, or a runtime that cannot be built, disables messages with the
/// cause. One clock origin is taken here, at the I/O boundary; the committed
/// cursor is read once and is both history's replay gate and the service's
/// startup fence. Replay runs in the service's task: the socket does not wait
/// for it.
fn start_messages(
    services: &mut methods::MethodServices,
    cancel: &CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    use crate::org::{
        message_evidence::MessageEvidence,
        message_history::MessageHistoryHandle,
        messages::{spawn_messages, MessageClock, MessageError, MessageHandle, MessageRuntime},
    };
    if services.messages.is_some() {
        return None;
    }
    let started = match services.delivery_policy.clone()? {
        Err(cause) => Err(cause),
        Ok(policy) => (|| {
            let control = services.control.clone().ok_or_else(|| {
                MessageError::Unavailable("control service not configured".into())
            })?;
            let high_water = services.org.status()?;
            let clock = MessageClock::new(tokio::time::Instant::now(), chrono::Utc::now());
            let history = MessageHistoryHandle::new(high_water, clock.now()?, policy.clone())?;
            let evidence = MessageEvidence::new(clock);
            let runtime = MessageRuntime::new(
                services.org.clone(),
                services.registry.clone(),
                control,
                services.agents.clone().unwrap_or_else(|| {
                    crate::org::agents::AgentHandle::disabled("agent service not configured")
                }),
                evidence.clone(),
                history.clone(),
                policy,
                cancel.clone(),
            );
            let (messages, task) = spawn_messages(runtime, high_water);
            Ok::<_, MessageError>((messages, task, (evidence, history)))
        })()
        .map_err(|error| error.to_string()),
    };
    match started {
        Ok((messages, task, tap)) => {
            services.messages = Some(messages);
            services.message_tap = Some(tap);
            Some(task)
        }
        Err(cause) => {
            warn!(%cause, "Message delivery is unavailable");
            services.messages = Some(MessageHandle::disabled(cause));
            None
        }
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

    #[error("Method registration failed: {0}")]
    Methods(#[from] methods::MethodError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn message_evidence_connections_use_enriched_services() {
        use crate::org::{
            message_evidence::{AttemptScope, MessageEvidence},
            message_history::MessageHistoryHandle,
            messages::MessageClock,
        };
        use atm_core::{AgentType, Harness, Model, SessionDomain};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let registry = crate::registry::spawn_registry();
        let mut row = SessionDomain::new(
            SessionId::new("wired"),
            AgentType::GeneralPurpose,
            Model::Unknown,
        );
        row.harness = Harness::Pi;
        row.tmux_pane = Some("%7".into());
        registry.register(row).await.unwrap();
        let mut services = methods::MethodServices::new(registry);
        let cancel = CancellationToken::new();
        let server = DaemonServer::with_services(
            "unused-private-test-socket",
            services.clone(),
            cancel.clone(),
        );
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
                harness: Harness::Pi,
                session_id: Some(SessionId::new("wired")),
                excluded_sessions: Default::default(),
            })
            .unwrap();
        // Runtime enriches its clone after DaemonServer construction.
        let history = MessageHistoryHandle::new(
            0,
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            atm_org::delivery::DeliveryPolicy::default(),
        )
        .unwrap();
        services.message_tap = Some((evidence.clone(), history));
        let (ours, peer) = tokio::net::UnixStream::pair().unwrap();
        let mut tasks = JoinSet::new();
        server.handle_connection(
            &mut tasks,
            ours,
            1,
            Arc::new(methods::router(services.clone()).unwrap()),
            &services,
        );
        let (read, mut write) = peer.into_split();
        let mut read = BufReader::new(read);
        // Synthetic envelope, same schema as the captured pi input.
        for message in [
            atm_protocol::ClientMessage::connect(None),
            atm_protocol::ClientMessage::pi_event(
                serde_json::json!({"event":"input","session_id":"wired","tmux_pane":"%7","payload":{"type":"input","source":"interactive","text":"[m123] Read: atm inbox --id m123"}}),
            ),
            atm_protocol::ClientMessage::ping(19),
        ] {
            write
                .write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes())
                .await
                .unwrap();
        }
        for _ in 0..2 {
            let mut line = String::new();
            timeout(Duration::from_secs(60), read.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            let message: atm_protocol::DaemonMessage = serde_json::from_str(&line).unwrap();
            assert!(matches!(
                message,
                atm_protocol::DaemonMessage::Connected { .. }
                    | atm_protocol::DaemonMessage::Pong { seq: 19 }
            ));
        }
        assert_eq!(
            evidence.receipt(id).unwrap().unwrap().session_id.as_str(),
            "wired"
        );
        cancel.cancel();
        while tasks.join_next().await.is_some() {}
    }

    /// Why the installed message handle refuses, if it does.
    async fn message_cause(services: &methods::MethodServices) -> Option<String> {
        use crate::org::messages::MessageError;
        match services.messages.as_ref()?.initialized().await {
            Err(MessageError::Unavailable(cause)) => Some(cause),
            _ => None,
        }
    }

    #[tokio::test]
    async fn messages_start_only_on_a_valid_policy_and_keep_an_injected_handle() {
        use crate::org::messages::MessageHandle;
        let cancel = CancellationToken::new();
        let policy = || Some(Ok(atm_org::delivery::DeliveryPolicy::default()));
        let fresh = || methods::MethodServices::new(crate::registry::spawn_registry());

        // Unconfigured: the router's own "not configured" handle stays.
        let mut services = fresh();
        assert!(start_messages(&mut services, &cancel).is_none());
        assert!(services.messages.is_none() && services.message_tap.is_none());

        // An invalid policy disables the methods with its cause.
        services.delivery_policy = Some(Err("invalid [delivery] settings".into()));
        assert!(start_messages(&mut services, &cancel).is_none());
        assert_eq!(
            message_cause(&services).await.as_deref(),
            Some("invalid [delivery] settings")
        );
        assert!(services.message_tap.is_none());

        // An injected handle is authoritative, whatever the policy says.
        let mut services = fresh();
        services.delivery_policy = policy();
        services.messages = Some(MessageHandle::disabled("injected"));
        assert!(start_messages(&mut services, &cancel).is_none());
        assert_eq!(message_cause(&services).await.as_deref(), Some("injected"));
        assert!(services.message_tap.is_none());

        // A valid policy with nothing to run on is disabled with that cause,
        // never half started: no control, then no organisation writer.
        let mut services = fresh();
        services.delivery_policy = policy();
        assert!(start_messages(&mut services, &cancel).is_none());
        let cause = message_cause(&services).await.unwrap();
        assert!(cause.contains("control service not configured"), "{cause}");
        let mut services = fresh();
        services.delivery_policy = policy();
        let (control, _task) = crate::control::service::spawn_control(
            Box::new(atm_tmux::MockTmuxClient::new()),
            services.registry.clone(),
            services.org.clone(),
            cancel.clone(),
        );
        services.control = Some(control);
        assert!(start_messages(&mut services, &cancel).is_none());
        let cause = message_cause(&services).await.unwrap();
        assert!(
            cause.contains("organisation service not configured"),
            "{cause}"
        );
        assert!(services.message_tap.is_none());
        cancel.cancel();
    }

    #[tokio::test]
    async fn started_messages_serve_the_router_install_the_tap_and_stop_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let mut services = methods::MethodServices::new(crate::registry::spawn_registry());
        services.org = crate::org::OrgHandle::open(dir.path().into(), cancel.clone()).await;
        let (control, _task) = crate::control::service::spawn_control(
            Box::new(atm_tmux::MockTmuxClient::new()),
            services.registry.clone(),
            services.org.clone(),
            cancel.clone(),
        );
        services.control = Some(control);
        services.delivery_policy = Some(Ok(atm_org::delivery::DeliveryPolicy::default()));
        let stop = CancellationToken::new();
        // Synchronous: the listener never waits for the replay.
        let task = start_messages(&mut services, &stop).unwrap();
        let (evidence, _history) = services.message_tap.clone().unwrap();
        assert!(evidence.available());
        // The router a connection is given serves the started service.
        let router = methods::router(services.clone()).unwrap();
        let caller = || methods::CallContext {
            client_id: methods::CallerId("test".into()),
            from_pane: None,
        };
        let call = |name: &'static str, params: serde_json::Value| {
            let router = &router;
            async move {
                timeout(Duration::from_secs(60), router.call(caller(), name, params))
                    .await
                    .unwrap()
            }
        };
        let sent = call(
            "msg.send",
            serde_json::json!({"to": "owner", "body": "hello"}),
        )
        .await
        .unwrap();
        let inbox = call("msg.inbox", serde_json::json!({})).await.unwrap();
        assert_eq!(inbox["messages"][0]["message_id"], sent["message_id"]);
        assert_eq!(inbox["messages"][0]["content"]["body"], "hello");
        assert_eq!(inbox["unread_count"], 1);
        // Messages stop on their own token, while control and the writer
        // (still running here) outlive them.
        stop.cancel();
        timeout(Duration::from_secs(60), task)
            .await
            .unwrap()
            .unwrap();
        let stopped = call("msg.inbox", serde_json::json!({})).await.unwrap_err();
        assert_eq!(stopped.0.code, "unavailable");
        assert!(services.org.status().is_ok());
        cancel.cancel();
    }

    /// A restart reads the log the previous process left. Copying it gives
    /// the next writer its own stream lock.
    fn copy_log(from: &std::path::Path, to: &std::path::Path) {
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
                copy_log(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn started_messages_hold_and_expect_an_attempt_of_the_previous_process() {
        use crate::org::messages::{Address, InboxParams, MessageContent, MessageId, MessageKind};
        use atm_org::delivery::{next_delivery, DeliveryPolicy, DeliveryStep, FailureReason};
        use atm_org::{DeliveryStatus, OrgCommand};
        let hang = Duration::from_secs(60);
        let worker: atm_org::AgentId = "demo/worker".parse().unwrap();

        // The previous process: a message to a bound agent, attempted on its
        // pane and never confirmed.
        let before = tempfile::tempdir().unwrap();
        let previous = CancellationToken::new();
        let org = crate::org::OrgHandle::open(before.path().into(), previous.clone()).await;
        let run = |command| {
            let org = org.clone();
            async move {
                let committed = timeout(hang, org.execute(command, chrono::Utc::now())).await;
                committed.unwrap().unwrap().last().unwrap().seq
            }
        };
        run(OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: "/demo".into(),
            move_root: false,
        })
        .await;
        run(OrgCommand::Adopt {
            agent_id: worker.clone(),
            pane: "%1".into(),
            session_id: Some("s1".into()),
        })
        .await;
        run(OrgCommand::RecordSummary(atm_org::SessionSummary {
            session_id: "s1".into(),
            pane: Some("%1".into()),
            agent_id: Some(worker.clone()),
            harness: atm_core::Harness::Pi,
            model: None,
            cost: None,
            first_prompt: None,
            started: chrono::Utc::now(),
            ended: None,
        }))
        .await;
        let sent = run(OrgCommand::SendMessage(MessageContent {
            from: Address::Owner,
            to: Address::Agent(worker.clone()),
            kind: MessageKind::Note,
            body: "brief".into(),
            attachments: vec![],
            task: None,
        }))
        .await;
        let id = MessageId::new(sent);
        run(OrgCommand::DeliveryAttempted {
            message_id: id,
            attempt: 1,
            pane: "%1".into(),
        })
        .await;
        previous.cancel();

        // This process, started as the daemon starts it.
        let after = tempfile::tempdir().unwrap();
        copy_log(before.path(), after.path());
        let cancel = CancellationToken::new();
        let mut services = methods::MethodServices::new(crate::registry::spawn_registry());
        services.org = crate::org::OrgHandle::open(after.path().into(), cancel.clone()).await;
        let (control, _task) = crate::control::service::spawn_control(
            Box::new(atm_tmux::MockTmuxClient::new()),
            services.registry.clone(),
            services.org.clone(),
            cancel.clone(),
        );
        services.control = Some(control);
        let policy = DeliveryPolicy::default();
        services.delivery_policy = Some(Ok(policy.clone()));
        let stop = CancellationToken::new();
        let task = start_messages(&mut services, &stop).unwrap();
        let messages = services.messages.clone().unwrap();
        let (evidence, history) = services.message_tap.clone().unwrap();
        timeout(hang, messages.initialized())
            .await
            .unwrap()
            .unwrap();

        // Initialized means replayed: the inbox has the message as the
        // previous process left it.
        let params = InboxParams {
            recipient: Some(Address::Agent(worker.clone())),
            unread_only: false,
            count_only: false,
            id: Some(id),
            after: None,
            limit: 1,
        };
        let caller = methods::CallContext {
            client_id: methods::CallerId("test".into()),
            from_pane: None,
        };
        let inbox = timeout(hang, messages.inbox(params, caller)).await.unwrap();
        let entry = inbox.unwrap().messages.remove(0);
        assert_eq!(entry.content.body, "brief");
        assert_eq!(entry.status, DeliveryStatus::Delivering);
        assert_eq!(entry.delivery.attempt, 1);
        // Its attempt is held for the owner: this process cannot know what
        // the paste did, so the policy must not paste it again.
        let held = || {
            let now = evidence.clock().now().unwrap();
            let snapshot = history.snapshot(id, now).unwrap();
            next_delivery(&snapshot.message, &snapshot.live, now, &policy).unwrap()
        };
        let restarted = DeliveryStep::Blocked {
            reason: FailureReason::Restarted,
        };
        assert_eq!(held(), restarted);
        // And the replay came before the service was ready: evidence expects
        // the attempted session again, on the clock history recovered with,
        // so its late receipt confirms.
        let recovered = evidence.clock().now().unwrap() - chrono::Utc::now();
        assert!(recovered.abs() < chrono::TimeDelta::hours(1), "{recovered}");
        let prompt = atm_core::LifecycleEvent::PromptSubmit {
            prompt: Some(format!("[{id}] Read: atm inbox --id {id}")),
        };
        evidence
            .tap(
                &SessionId::new("s1"),
                Some("%1"),
                Some(atm_core::Harness::Pi),
                Some(&prompt),
                true,
                Some(&history),
            )
            .unwrap();
        assert_eq!(
            evidence.receipt(id).unwrap().unwrap().session_id.as_str(),
            "s1"
        );
        // Messages stop while the writer they commit to is still there.
        stop.cancel();
        timeout(hang, task).await.unwrap().unwrap();
        cancel.cancel();
    }

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
