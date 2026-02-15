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

    /// Status line update from Claude Code
    StatusUpdate {
        /// The raw status line JSON (to be parsed)
        data: serde_json::Value,
    },

    /// Hook event from Claude Code
    HookEvent {
        /// The raw hook event JSON (to be parsed)
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

    /// Request daemon to discover existing Claude sessions
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

    /// Creates a status update message.
    pub fn status_update(data: serde_json::Value) -> Self {
        Self::new(MessageType::StatusUpdate { data })
    }

    /// Creates a hook event message.
    pub fn hook_event(data: serde_json::Value) -> Self {
        Self::new(MessageType::HookEvent { data })
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
