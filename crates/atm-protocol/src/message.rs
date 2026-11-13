//! Protocol message types for daemon communication.

use crate::version::ProtocolVersion;
use atm_core::{SessionId, SessionView};
use serde::{Deserialize, Serialize};

/// Message types that can be sent by clients to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageType {
    /// Client handshake/connection request
    Connect {
        /// Client identifier (optional)
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },

    /// Invoke a control-plane method.
    Request {
        /// Caller-supplied correlation ID.
        id: String,
        /// Method name advertised by the daemon.
        method: String,
        /// Method-specific parameters; omitted means null.
        #[serde(default)]
        params: serde_json::Value,
        /// Caller's tmux pane, when available.
        #[serde(skip_serializing_if = "Option::is_none")]
        from_pane: Option<String>,
    },

    /// Replay events after a cursor, then follow live events.
    Watch {
        /// Event-kind prefixes; empty means all events.
        #[serde(default)]
        topics: Vec<String>,
        /// Last sequence seen by the caller; required even for a new watch.
        cursor: u64,
    },

    /// Stop watching events.
    Unwatch,

    /// Status line update from Claude Code (raw `RawStatusLine` payload).
    /// Parsed by `atm-claude-adapter` at the daemon boundary.
    StatusUpdate {
        /// The raw status line JSON (to be parsed)
        data: serde_json::Value,
    },

    /// Hook event from Claude Code (raw `RawHookEvent` payload).
    /// Translated by `atm-claude-adapter` at the daemon boundary.
    HookEvent {
        /// The raw hook event JSON (to be parsed)
        data: serde_json::Value,
    },

    /// Event from a pi extension (raw `RawPiEvent` payload).
    /// Translated by `atm-pi-adapter` at the daemon boundary.
    /// Symmetric with [`Self::HookEvent`].
    PiEvent {
        /// The raw pi event JSON (to be parsed)
        data: serde_json::Value,
    },

    /// Hook event from the Codex CLI (raw `RawCodexEvent` payload).
    /// Translated by `atm-codex-adapter` at the daemon boundary.
    /// Symmetric with [`Self::HookEvent`] and [`Self::PiEvent`].
    CodexEvent {
        /// The raw codex event JSON (to be parsed)
        data: serde_json::Value,
    },

    /// Request current session list
    ListSessions,

    /// Subscribe to session updates
    Subscribe {
        /// Optional filter by session ID
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },

    /// Unsubscribe from updates
    Unsubscribe,

    /// Ping to check connection
    Ping {
        /// Sequence number for matching pong response
        seq: u64,
    },

    /// Client disconnecting gracefully
    Disconnect,

    /// Request daemon to discover existing sessions of each built-in
    /// harness with
    /// [`discovery_enabled`](atm_core::HarnessDefinition::discovery_enabled)
    Discover,
}

/// Messages sent from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    /// Protocol version
    pub protocol_version: ProtocolVersion,

    /// Message payload
    #[serde(flatten)]
    pub message: MessageType,
}

impl ClientMessage {
    /// Creates a new client message with current protocol version.
    pub fn new(message: MessageType) -> Self {
        Self {
            protocol_version: ProtocolVersion::CURRENT,
            message,
        }
    }

    /// Creates a connect message.
    pub fn connect(client_id: Option<String>) -> Self {
        Self::new(MessageType::Connect { client_id })
    }

    /// Creates a control-plane request.
    #[must_use]
    pub fn request(
        id: String,
        method: String,
        params: serde_json::Value,
        from_pane: Option<String>,
    ) -> Self {
        Self::new(MessageType::Request {
            id,
            method,
            params,
            from_pane,
        })
    }

    /// Creates an event watch starting after the supplied cursor.
    #[must_use]
    pub fn watch(topics: Vec<String>, cursor: u64) -> Self {
        Self::new(MessageType::Watch { topics, cursor })
    }

    /// Creates a request to stop watching events.
    #[must_use]
    pub fn unwatch() -> Self {
        Self::new(MessageType::Unwatch)
    }

    /// Creates a status update message.
    pub fn status_update(data: serde_json::Value) -> Self {
        Self::new(MessageType::StatusUpdate { data })
    }

    /// Creates a hook event message (Claude raw payload).
    pub fn hook_event(data: serde_json::Value) -> Self {
        Self::new(MessageType::HookEvent { data })
    }

    /// Creates a pi event message (raw pi-extension payload).
    pub fn pi_event(data: serde_json::Value) -> Self {
        Self::new(MessageType::PiEvent { data })
    }

    /// Creates a codex event message (raw Codex hook payload).
    pub fn codex_event(data: serde_json::Value) -> Self {
        Self::new(MessageType::CodexEvent { data })
    }

    /// Creates a list sessions request.
    pub fn list_sessions() -> Self {
        Self::new(MessageType::ListSessions)
    }

    /// Creates a subscribe message.
    pub fn subscribe(session_id: Option<SessionId>) -> Self {
        Self::new(MessageType::Subscribe { session_id })
    }

    /// Creates a ping message.
    pub fn ping(seq: u64) -> Self {
        Self::new(MessageType::Ping { seq })
    }

    /// Creates a disconnect message.
    pub fn disconnect() -> Self {
        Self::new(MessageType::Disconnect)
    }

    /// Creates a discover message.
    pub fn discover() -> Self {
        Self::new(MessageType::Discover)
    }
}

/// A control-plane method failure. Codes are open strings; see [`crate::error_code`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    /// Machine-readable error code.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
}

/// Exactly one result or error, flattened into a response's wire object.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayload {
    /// Successful result, including JSON null.
    Result(serde_json::Value),
    /// Method failure.
    Error(ResponseError),
}

impl<'de> Deserialize<'de> for ResponsePayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PayloadVisitor;

        impl<'de> serde::de::Visitor<'de> for PayloadVisitor {
            type Value = ResponsePayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("exactly one of result or error")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut payload = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "result" | "error" => {
                            if payload.is_some() {
                                return Err(serde::de::Error::custom(
                                    "response must contain exactly one of result or error",
                                ));
                            }
                            payload = Some(if key == "result" {
                                ResponsePayload::Result(map.next_value()?)
                            } else {
                                ResponsePayload::Error(map.next_value()?)
                            });
                        }
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                payload.ok_or_else(|| {
                    serde::de::Error::custom("response must contain exactly one of result or error")
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

/// Messages sent from daemon to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// Connection accepted
    Connected {
        /// Daemon's protocol version
        protocol_version: ProtocolVersion,
        /// Assigned client ID
        client_id: String,
        /// Supported control-plane methods; absent on protocol 1.0 daemons.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        methods: Vec<String>,
    },

    /// Reply to a control-plane request.
    Response {
        /// Request correlation ID.
        id: String,
        /// Exactly one result or error on the wire.
        #[serde(flatten)]
        payload: ResponsePayload,
    },

    /// One event from a watched topic.
    Event {
        /// Event stream topic.
        topic: String,
        /// Sequence number used for replay cursors.
        seq: u64,
        /// Caller-supplied timestamp string.
        ts: String,
        /// Event kind, matched against watch prefixes.
        kind: String,
        /// Event-specific data.
        body: serde_json::Value,
    },

    /// Connection rejected (version mismatch, etc.)
    Rejected {
        /// Reason for rejection
        reason: String,
        /// Daemon's protocol version (for client to upgrade)
        protocol_version: ProtocolVersion,
    },

    /// Full session list response
    SessionList {
        /// All current sessions
        sessions: Vec<SessionView>,
    },

    /// Session was created or updated
    SessionUpdated {
        /// The updated session
        session: Box<SessionView>,
    },

    /// Session was removed (stale, disconnected)
    SessionRemoved {
        /// ID of the removed session
        session_id: SessionId,
    },

    /// Pong response to ping
    Pong {
        /// Sequence number from ping
        seq: u64,
    },

    /// Error response
    Error {
        /// Error message
        message: String,
        /// Error code (optional)
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },

    /// Discovery completed response
    DiscoveryComplete {
        /// Number of sessions discovered
        discovered: u32,
        /// Number of discovery failures (logged at debug)
        failed: u32,
    },
}

impl DaemonMessage {
    /// Creates a connected response.
    pub fn connected(client_id: String) -> Self {
        Self::Connected {
            protocol_version: ProtocolVersion::CURRENT,
            client_id,
            methods: Vec::new(),
        }
    }

    /// Creates a successful control-plane response, including a null result.
    #[must_use]
    pub fn response_ok(id: String, value: serde_json::Value) -> Self {
        Self::Response {
            id,
            payload: ResponsePayload::Result(value),
        }
    }

    /// Creates a failed control-plane response.
    #[must_use]
    pub fn response_err(id: String, code: &str, message: &str) -> Self {
        Self::Response {
            id,
            payload: ResponsePayload::Error(ResponseError {
                code: code.to_string(),
                message: message.to_string(),
            }),
        }
    }

    /// Creates an event using the supplied sequence and timestamp.
    #[must_use]
    pub fn event(
        topic: String,
        seq: u64,
        ts: String,
        kind: String,
        body: serde_json::Value,
    ) -> Self {
        Self::Event {
            topic,
            seq,
            ts,
            kind,
            body,
        }
    }

    /// Creates a rejected response.
    pub fn rejected(reason: &str) -> Self {
        Self::Rejected {
            reason: reason.to_string(),
            protocol_version: ProtocolVersion::CURRENT,
        }
    }

    /// Creates a session list response.
    pub fn session_list(sessions: Vec<SessionView>) -> Self {
        Self::SessionList { sessions }
    }

    /// Creates a session updated notification.
    pub fn session_updated(session: SessionView) -> Self {
        Self::SessionUpdated {
            session: Box::new(session),
        }
    }

    /// Creates a session removed notification.
    pub fn session_removed(session_id: SessionId) -> Self {
        Self::SessionRemoved { session_id }
    }

    /// Creates a pong response.
    pub fn pong(seq: u64) -> Self {
        Self::Pong { seq }
    }

    /// Creates an error response.
    pub fn error(message: &str) -> Self {
        Self::Error {
            message: message.to_string(),
            code: None,
        }
    }

    /// Creates an error response with code.
    pub fn error_with_code(message: &str, code: &str) -> Self {
        Self::Error {
            message: message.to_string(),
            code: Some(code.to_string()),
        }
    }

    /// Creates a discovery complete response.
    pub fn discovery_complete(discovered: u32, failed: u32) -> Self {
        Self::DiscoveryComplete { discovered, failed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_message_serialization() {
        let msg = ClientMessage::ping(42);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"ping\""));
        assert!(json.contains("\"seq\":42"));
    }

    #[test]
    fn test_daemon_message_serialization() {
        let msg = DaemonMessage::connected("client-123".to_string());
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"connected\""));
        assert!(json.contains("\"client_id\":\"client-123\""));
    }

    #[test]
    fn test_codex_event_roundtrip() {
        let original = ClientMessage::codex_event(serde_json::json!({
            "session_id": "s-1",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash"
        }));
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("\"type\":\"codex_event\""));

        let parsed: ClientMessage = serde_json::from_str(&json).unwrap();
        match parsed.message {
            MessageType::CodexEvent { data } => {
                assert_eq!(
                    data.get("hook_event_name").and_then(|v| v.as_str()),
                    Some("PreToolUse")
                );
            }
            _ => panic!("Expected CodexEvent message"),
        }
    }

    #[test]
    fn test_message_roundtrip() {
        let original = ClientMessage::subscribe(Some(SessionId::new("test-session")));
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ClientMessage = serde_json::from_str(&json).unwrap();

        match parsed.message {
            MessageType::Subscribe { session_id } => {
                assert_eq!(
                    session_id.map(|s| s.as_str().to_string()),
                    Some("test-session".to_string())
                );
            }
            _ => panic!("Expected Subscribe message"),
        }
    }
}
