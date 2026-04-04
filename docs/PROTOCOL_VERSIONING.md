# Agent Tmux Manager Protocol Versioning

> **⚠️ Panic-Free Policy:** All code in this document follows the panic-free guidelines from `CLAUDE.md`.
> No `.unwrap()`, `.expect()`, `panic!()`, or direct indexing in production code.
> Use `?`, `.ok()`, `.unwrap_or()`, `.unwrap_or_default()`, or pattern matching instead.
> Exception: `.expect()` is allowed for compile-time known-valid literals (documented with SAFETY comment).

## Overview

This document specifies the protocol versioning strategy for the ATM daemon communication protocol. Versioning ensures backward compatibility and enables smooth upgrades as the protocol evolves.

---

## Version Format

**Semantic Versioning:** `MAJOR.MINOR`

- **MAJOR:** Breaking changes (incompatible message formats)
- **MINOR:** Backward-compatible additions (new optional fields)

**Current Version:** `1.0`

### Version String Format

```
MAJOR.MINOR
```

Examples:
- `1.0` - Initial release
- `1.1` - Added optional fields
- `2.0` - Breaking changes

---

## Message Format

### All Messages Include Version

Every protocol message includes a `protocol_version` field:

```json
{
  "protocol_version": "1.0",
  "type": "register",
  "session_id": "abc123",
  "agent_type": "claude-code",
  "pid": 12345
}
```

### Rust Message Types

```rust
use serde::{Deserialize, Serialize};

/// Base trait for all protocol messages
pub trait ProtocolMessage {
    fn protocol_version(&self) -> &str;
}

/// Client-to-daemon message wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    pub protocol_version: String,

    #[serde(flatten)]
    pub payload: ClientPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientPayload {
    Register {
        session_id: String,
        agent_type: String,
        pid: u32,
    },
    StatusUpdate {
        session_id: String,
        context: ContextUsage,
    },
    Unregister {
        session_id: String,
    },
}

/// Daemon-to-client message wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonMessage {
    pub protocol_version: String,

    #[serde(flatten)]
    pub payload: DaemonPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonPayload {
    Ack {
        success: bool,
        message: Option<String>,
    },
    SessionList {
        sessions: Vec<SessionView>,
    },
    SessionUpdate {
        session: SessionView,
    },
    Error {
        code: String,
        message: String,
    },
}
```

---

## Version in First Message Optimization

For status updates (sent every 300ms), version overhead is expensive. Instead, we use a handshake-based approach:

### Handshake: First Message Includes Version

```json
{
  "protocol_version": "1.0",
  "type": "register",
  "session_id": "abc123",
  "agent_type": "claude-code",
  "pid": 12345,
  "cwd": "/home/user/project"
}
```

### Subsequent Messages: Version Omitted

After successful registration, subsequent messages from the same session can omit the version field (assumed same as handshake):

```json
{
  "type": "status_update",
  "session_id": "abc123",
  "context": {
    "used_percentage": 45.5,
    "total_read": 15000,
    "session_cost_usd": 0.05
  }
}
```

### Implementation

```rust
use std::collections::HashMap;

pub struct ConnectionState {
    /// Protocol version established during handshake
    session_versions: HashMap<String, String>,
}

impl ConnectionState {
    pub fn new() -> Self {
        Self {
            session_versions: HashMap::new(),
        }
    }

    /// Record the protocol version for a session during registration
    pub fn register_session(&mut self, session_id: &str, protocol_version: &str) {
        self.session_versions.insert(
            session_id.to_string(),
            protocol_version.to_string(),
        );
    }

    /// Get the protocol version for a session
    pub fn get_version(&self, session_id: &str) -> Option<&str> {
        self.session_versions.get(session_id).map(|s| s.as_str())
    }

    /// Parse a message, using stored version if not present
    pub fn parse_message(&self, line: &str) -> Result<(String, ClientPayload), ProtocolError> {
        // First try to parse with version
        if let Ok(msg) = serde_json::from_str::<ClientMessage>(line) {
            return Ok((msg.protocol_version, msg.payload));
        }

        // Try to parse without version (status update optimization)
        let partial: PartialMessage = serde_json::from_str(line)?;

        // Look up version from session state
        if let Some(version) = self.get_version(&partial.session_id) {
            let payload: ClientPayload = serde_json::from_str(line)?;
            return Ok((version.to_string(), payload));
        }

        Err(ProtocolError::MissingVersion)
    }
}

#[derive(Deserialize)]
struct PartialMessage {
    session_id: String,
}
```

---

## Version Negotiation

### Client Hello (Client to Daemon)

When a client connects, it sends a hello message:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    /// Protocol version the client supports
    pub protocol_version: String,

    /// Type of client connecting
    pub client_type: ClientType,

    /// Optional client capabilities for future extensibility
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientType {
    /// Claude Code session (bash script integration)
    Session,

    /// TUI dashboard client
    Tui,

    /// CLI tool (one-shot commands)
    Cli,
}
```

**JSON Example:**

```json
{
  "protocol_version": "1.0",
  "client_type": "session",
  "capabilities": ["streaming", "batch_updates"]
}
```

### Server Hello (Daemon Response)

The daemon responds with acceptance or rejection:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    /// Protocol version the server will use
    pub protocol_version: String,

    /// Whether the connection is accepted
    pub accepted: bool,

    /// Reason for rejection (if not accepted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Server capabilities
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
}
```

**Accepted Response:**

```json
{
  "protocol_version": "1.0",
  "accepted": true,
  "capabilities": ["streaming", "batch_updates", "compression"]
}
```

**Rejected Response:**

```json
{
  "protocol_version": "2.0",
  "accepted": false,
  "reason": "Unsupported protocol version 1.0. Please upgrade to 2.0+"
}
```

### Negotiation Implementation

```rust
use semver::{Version, VersionReq};

pub struct ProtocolNegotiator {
    /// Version requirement for this daemon
    supported_versions: VersionReq,

    /// Current daemon version
    current_version: Version,
}

impl ProtocolNegotiator {
    pub fn new() -> Self {
        Self {
            // Accept 1.x versions
            // SAFETY: These are compile-time known-valid semver literals
            supported_versions: VersionReq::parse(">=1.0.0, <2.0.0")
                .expect("valid semver requirement literal"),
            current_version: Version::parse("1.0.0")
                .expect("valid semver literal"),
        }
    }

    pub fn negotiate(&self, client_hello: &ClientHello) -> ServerHello {
        // Parse client version
        let client_version = match Version::parse(&format!("{}.0", client_hello.protocol_version)) {
            Ok(v) => v,
            Err(_) => {
                return ServerHello {
                    protocol_version: format!("{}.{}",
                        self.current_version.major,
                        self.current_version.minor),
                    accepted: false,
                    reason: Some(format!(
                        "Invalid version format: {}. Expected MAJOR.MINOR",
                        client_hello.protocol_version
                    )),
                    capabilities: None,
                };
            }
        };

        // Check compatibility
        if self.supported_versions.matches(&client_version) {
            ServerHello {
                protocol_version: format!("{}.{}",
                    self.current_version.major,
                    self.current_version.minor),
                accepted: true,
                reason: None,
                capabilities: Some(vec![
                    "streaming".to_string(),
                    "batch_updates".to_string(),
                ]),
            }
        } else {
            ServerHello {
                protocol_version: format!("{}.{}",
                    self.current_version.major,
                    self.current_version.minor),
                accepted: false,
                reason: Some(format!(
                    "Unsupported protocol version {}. Daemon requires {}",
                    client_hello.protocol_version,
                    self.supported_versions
                )),
                capabilities: None,
            }
        }
    }
}
```

---

## Compatibility Rules

### Version Matching Table

| Daemon Version | Accepts Client Versions | Rejects |
|---------------|------------------------|---------|
| 1.0 | 1.0, 1.1, 1.x | 2.0+ |
| 1.1 | 1.0, 1.1, 1.x | 2.0+ |
| 2.0 | 2.0, 2.x | 1.x (see migration) |

### Major Version Rules

1. **Same major version:** Always compatible
2. **Different major version:** Incompatible by default
3. **Migration period:** New major versions should support previous major for 6 months

### Minor Version Rules

1. **Higher minor:** Server accepts (may ignore new fields)
2. **Lower minor:** Server accepts (provides defaults for missing fields)
3. **Unknown fields:** Ignored by default (serde `#[serde(deny_unknown_fields)]` NOT used)

---

## Backward Compatibility Strategy

### Adding Optional Fields (Minor Version Bump)

**Example:** Adding `memory_pressure` in v1.1

**v1.0 Session struct:**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionV1_0 {
    pub session_id: String,
    pub status: SessionStatus,
    pub context: ContextUsage,
    pub agent_type: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}
```

**v1.1 Session struct:**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub status: SessionStatus,
    pub context: ContextUsage,
    pub agent_type: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,

    // New in v1.1 - optional for backward compatibility
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_pressure: Option<f64>,

    // New in v1.1
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_usage: Option<f64>,
}
```

**Compatibility:**
- **v1.0 clients:** Ignore unknown fields (`memory_pressure`, `cpu_usage`)
- **v1.1 clients:** Use new fields if present

**Parsing v1.0 messages in v1.1 daemon:**

```rust
impl Session {
    /// Create session with defaults for v1.0 compatibility
    pub fn from_v1_0(v1: SessionV1_0) -> Self {
        Self {
            session_id: v1.session_id,
            status: v1.status,
            context: v1.context,
            agent_type: v1.agent_type,
            pid: v1.pid,
            started_at: v1.started_at,
            memory_pressure: None,  // Not available in v1.0
            cpu_usage: None,        // Not available in v1.0
        }
    }
}
```

### Breaking Changes (Major Version Bump)

**Example:** Renaming field in v2.0

**v1.x message:**

```json
{
  "protocol_version": "1.0",
  "type": "register",
  "agent_type": "general-purpose"
}
```

**v2.0 message:**

```json
{
  "protocol_version": "2.0",
  "type": "register",
  "agent_kind": "general-purpose"
}
```

**Migration: Supporting both protocols during transition:**

```rust
#[derive(Debug, Clone)]
pub enum ProtocolVersion {
    V1,
    V2,
}

impl ProtocolVersion {
    pub fn parse(version_str: &str) -> Result<Self, ProtocolError> {
        let parts: Vec<&str> = version_str.split('.').collect();

        // Use .first() instead of [0] to avoid potential panic
        let major = parts.first()
            .ok_or_else(|| ProtocolError::InvalidVersion(version_str.to_string()))?;

        match *major {
            "1" => Ok(ProtocolVersion::V1),
            "2" => Ok(ProtocolVersion::V2),
            _ => Err(ProtocolError::UnsupportedVersion(version_str.to_string())),
        }
    }
}

/// Unified message handler for multi-version support
pub struct MultiVersionHandler {
    v1_handler: V1Handler,
    v2_handler: V2Handler,
}

impl MultiVersionHandler {
    /// Default timeout for handler operations
    const HANDLER_TIMEOUT: Duration = Duration::from_secs(5);

    pub async fn handle_message(
        &self,
        version: ProtocolVersion,
        line: &str
    ) -> Result<DaemonMessage, ProtocolError> {
        use tokio::time::timeout;

        match version {
            ProtocolVersion::V1 => {
                let msg: V1ClientMessage = serde_json::from_str(line)?;
                timeout(Self::HANDLER_TIMEOUT, self.v1_handler.handle(msg))
                    .await
                    .map_err(|_| ProtocolError::HandlerTimeout)?
            }
            ProtocolVersion::V2 => {
                let msg: V2ClientMessage = serde_json::from_str(line)?;
                timeout(Self::HANDLER_TIMEOUT, self.v2_handler.handle(msg))
                    .await
                    .map_err(|_| ProtocolError::HandlerTimeout)?
            }
        }
    }
}

/// V1 message types
pub mod v1 {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RegisterMessage {
        pub session_id: String,
        pub agent_type: String,  // v1 field name
        pub pid: u32,
    }
}

/// V2 message types
pub mod v2 {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RegisterMessage {
        pub session_id: String,
        pub agent_kind: String,  // v2 field name (renamed)
        pub pid: u32,
    }
}

/// Convert between versions
impl From<v1::RegisterMessage> for v2::RegisterMessage {
    fn from(v1: v1::RegisterMessage) -> Self {
        Self {
            session_id: v1.session_id,
            agent_kind: v1.agent_type,  // Field rename
            pid: v1.pid,
        }
    }
}
```

---

## Version Detection in Bash Scripts

### Status Line Script (atm-status.sh)

```bash
#!/bin/bash
# Agent Tmux Manager status line integration for Claude Code
# Protocol version: 1.0

set -euo pipefail

# Configuration
PROTOCOL_VERSION="1.0"
SOCKET="/tmp/atm.sock"
LOG_FILE="/tmp/atm-client.log"
TIMEOUT=0.1  # 100ms timeout for non-blocking

# Generate or retrieve session ID
if [ -n "${CLAUDE_SESSION_ID:-}" ]; then
    SESSION_ID="$CLAUDE_SESSION_ID"
elif [ -n "${ATM_SESSION_ID:-}" ]; then
    SESSION_ID="$ATM_SESSION_ID"
else
    SESSION_ID="claude-$(date +%s)-$$"
fi

# Session state file
STATE_FILE="/tmp/atm-session-$SESSION_ID"

# Log function (only on errors)
log_error() {
    echo "$(date -Iseconds): $1" >> "$LOG_FILE"
}

# Check if daemon is available
if [ ! -S "$SOCKET" ]; then
    # Daemon not running - silently exit
    exit 0
fi

# Send handshake on first connection
send_handshake() {
    local handshake
    handshake=$(cat <<EOF
{
  "protocol_version": "$PROTOCOL_VERSION",
  "type": "register",
  "session_id": "$SESSION_ID",
  "agent_type": "claude-code",
  "pid": $$,
  "cwd": "$(pwd)"
}
EOF
)

    if echo "$handshake" | timeout "$TIMEOUT" nc -U "$SOCKET" 2>/dev/null; then
        touch "$STATE_FILE"
        return 0
    else
        log_error "Failed to send handshake"
        return 1
    fi
}

# Send status update (no version field - optimization)
send_status_update() {
    local context_json="$1"
    local status_update
    status_update=$(cat <<EOF
{
  "type": "status_update",
  "session_id": "$SESSION_ID",
  "context": $context_json,
  "timestamp": "$(date -Iseconds)"
}
EOF
)

    echo "$status_update" | timeout "$TIMEOUT" nc -U "$SOCKET" 2>/dev/null || {
        log_error "Failed to send status update"
        return 1
    }
}

# Main: Read status line JSON from stdin
main() {
    # Send handshake if not already registered
    if [ ! -f "$STATE_FILE" ]; then
        send_handshake || exit 0
    fi

    # Read JSON from stdin (single read pattern per Claude Code docs)
    local input
    input=$(cat)

    if [ -n "$input" ]; then
        # Extract context data from status line
        # Note: Actual field names based on Claude Code integration validation
        local context_json
        context_json=$(echo "$input" | jq -c '{
            total_cost_usd: .cost.total_cost_usd,
            total_duration_secs: .cost.total_duration_secs,
            lines_read: .cost.lines_read
        }' 2>/dev/null) || context_json="{}"

        send_status_update "$context_json"
    fi
}

main "$@"
exit 0  # Always exit 0 to avoid breaking Claude Code
```

### Hooks Script (atm-hooks.sh)

```bash
#!/bin/bash
# Agent Tmux Manager hooks integration for Claude Code
# Protocol version: 1.0

set -euo pipefail

# Configuration
PROTOCOL_VERSION="1.0"
SOCKET="/tmp/atm.sock"
LOG_FILE="/tmp/atm-hooks.log"
TIMEOUT=0.1

# Session ID from environment or generate
SESSION_ID="${CLAUDE_SESSION_ID:-${ATM_SESSION_ID:-hook-$$}}"

# Log function
log_error() {
    echo "$(date -Iseconds): $1" >> "$LOG_FILE"
}

# Check daemon availability
if [ ! -S "$SOCKET" ]; then
    exit 0
fi

# Send hook event to daemon
send_hook_event() {
    local hook_event="$1"
    local tool_name="$2"
    local tool_input="$3"

    local message
    message=$(cat <<EOF
{
  "protocol_version": "$PROTOCOL_VERSION",
  "type": "hook_event",
  "session_id": "$SESSION_ID",
  "event": {
    "hook_name": "$hook_event",
    "tool_name": "$tool_name",
    "tool_input": $tool_input,
    "timestamp": "$(date -Iseconds)"
  }
}
EOF
)

    echo "$message" | timeout "$TIMEOUT" nc -U "$SOCKET" 2>/dev/null || {
        log_error "Failed to send hook event: $hook_event"
        return 1
    }
}

# Main: Read hook event from stdin
main() {
    local input
    input=$(cat)

    if [ -n "$input" ]; then
        # Parse hook event (based on Claude Code hook documentation)
        local hook_name
        local tool_name
        local tool_input

        hook_name=$(echo "$input" | jq -r '.hook_event_name // "unknown"')
        tool_name=$(echo "$input" | jq -r '.tool_name // "unknown"')
        tool_input=$(echo "$input" | jq -c '.tool_input // {}')

        send_hook_event "$hook_name" "$tool_name" "$tool_input"
    fi
}

main "$@"
exit 0  # Always exit 0
```

---

## Future Version Planning

### v1.1 (Planned Features)

Backward-compatible additions:

```rust
/// v1.1 additions to Session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionV1_1 {
    // Existing v1.0 fields
    pub session_id: String,
    pub status: SessionStatus,
    pub context: ContextUsage,
    pub agent_type: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,

    // v1.1 additions (all optional for backward compatibility)

    /// Memory pressure indicator (0.0-1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_pressure: Option<f64>,

    /// History of session events for debugging
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_history: Option<Vec<SessionEvent>>,
}

/// v1.1 additions to GetAllSessions request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetAllSessionsRequestV1_1 {
    // v1.1 addition: optional filtering
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<SessionFilter>,

    // v1.1 addition: optional sorting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<SortField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFilter {
    pub status: Option<SessionStatus>,
    pub agent_type: Option<String>,
    pub min_context_usage: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    StartedAt,
    ContextUsage,
    LastUpdate,
}
```

### v2.0 (Breaking Changes - Distant Future)

Major version changes planned:

1. **Change agent_type from String to enum in JSON**

```rust
// v1.x: "agent_type": "claude-code"
// v2.0: "agent_kind": { "type": "claude_code", "version": "1.0" }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode { version: String },
    Cursor { version: String },
    Custom { name: String, version: String },
}
```

2. **Rename fields for consistency**

```rust
// v1.x inconsistency:
//   session_id vs sessionId
//   agent_type vs agentType
//
// v2.0: All snake_case consistently
```

3. **Switch to binary protocol (msgpack/bincode) for performance**

```rust
use serde::{Deserialize, Serialize};
use rmp_serde as rmps;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryMessage {
    pub header: MessageHeader,
    pub payload: Vec<u8>,
}

impl BinaryMessage {
    pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, Error> {
        rmps::to_vec(msg)
    }

    pub fn decode<T: for<'de> Deserialize<'de>>(data: &[u8]) -> Result<T, Error> {
        rmps::from_slice(data)
    }
}
```

---

## Version Mismatch Error Messages

### For Users (TUI Display)

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum VersionError {
    #[error("Protocol version mismatch")]
    Mismatch {
        daemon_version: String,
        client_version: String,
    },

    #[error("Unsupported protocol version")]
    Unsupported { version: String },

    #[error("Invalid version format")]
    InvalidFormat { version: String },
}

impl VersionError {
    /// User-friendly error message for TUI display
    pub fn user_message(&self) -> String {
        match self {
            VersionError::Mismatch { daemon_version, client_version } => {
                format!(
                    "Error: Protocol version mismatch\n\
                     Daemon version: {}\n\
                     Client version: {}\n\n\
                     Please upgrade atm TUI:\n  \
                     cargo install atm --force",
                    daemon_version, client_version
                )
            }
            VersionError::Unsupported { version } => {
                format!(
                    "Error: Unsupported protocol version: {}\n\n\
                     Please check that both daemon and client are up to date:\n  \
                     cargo install atm --force\n  \
                     systemctl --user restart atmd",
                    version
                )
            }
            VersionError::InvalidFormat { version } => {
                format!(
                    "Error: Invalid version format: {}\n\
                     Expected format: MAJOR.MINOR (e.g., 1.0)",
                    version
                )
            }
        }
    }
}
```

**Example TUI Error Display:**

```
+--------------------------------------------------+
|  ERROR: Protocol Version Mismatch                |
|                                                  |
|  Daemon version: 2.0                             |
|  Client version: 1.0                             |
|                                                  |
|  Please upgrade atm TUI:                   |
|    cargo install atm --force               |
|                                                  |
|  Press 'q' to quit                               |
+--------------------------------------------------+
```

### For Developers (Log Messages)

```rust
use tracing::{warn, error, info};

/// Log version-related events for debugging
pub fn log_version_event(event: &VersionEvent) {
    match event {
        VersionEvent::Negotiation { client_version, accepted } => {
            info!(
                client_version = %client_version,
                accepted = %accepted,
                "Protocol version negotiation"
            );
        }
        VersionEvent::Mismatch { client_version, daemon_version, client_addr } => {
            warn!(
                client_version = %client_version,
                daemon_version = %daemon_version,
                client_addr = %client_addr,
                action = "rejected",
                "Version mismatch from client"
            );
        }
        VersionEvent::Upgrade { from_version, to_version } => {
            info!(
                from_version = %from_version,
                to_version = %to_version,
                "Client upgraded protocol version"
            );
        }
    }
}

pub enum VersionEvent {
    Negotiation {
        client_version: String,
        accepted: bool,
    },
    Mismatch {
        client_version: String,
        daemon_version: String,
        client_addr: String,
    },
    Upgrade {
        from_version: String,
        to_version: String,
    },
}
```

**Example Log Output:**

```
2024-01-15T10:23:45.123Z WARN atmd::protocol: Version mismatch from client
    client_version="1.0"
    daemon_version="2.0"
    client_addr="/tmp/atm.sock"
    action="rejected"

2024-01-15T10:23:50.456Z INFO atmd::protocol: Protocol version negotiation
    client_version="2.0"
    accepted=true
```

---

## Testing Protocol Versioning

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_parsing() {
        assert!(matches!(
            ProtocolVersion::parse("1.0"),
            Ok(ProtocolVersion::V1)
        ));
        assert!(matches!(
            ProtocolVersion::parse("1.5"),
            Ok(ProtocolVersion::V1)
        ));
        assert!(matches!(
            ProtocolVersion::parse("2.0"),
            Ok(ProtocolVersion::V2)
        ));
        assert!(matches!(
            ProtocolVersion::parse("3.0"),
            Err(ProtocolError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn test_version_negotiation_accept() {
        let negotiator = ProtocolNegotiator::new();
        let client_hello = ClientHello {
            protocol_version: "1.0".to_string(),
            client_type: ClientType::Tui,
            capabilities: None,
        };

        let response = negotiator.negotiate(&client_hello);
        assert!(response.accepted);
    }

    #[test]
    fn test_version_negotiation_reject() {
        let negotiator = ProtocolNegotiator::new();
        let client_hello = ClientHello {
            protocol_version: "3.0".to_string(),
            client_type: ClientType::Tui,
            capabilities: None,
        };

        let response = negotiator.negotiate(&client_hello);
        assert!(!response.accepted);
        assert!(response.reason.is_some());
    }

    #[test]
    fn test_optional_field_serialization() {
        // v1.1 session with new field
        let session = Session {
            session_id: "test-123".to_string(),
            status: SessionStatus::Active,
            context: ContextUsage::default(),
            agent_type: "claude-code".to_string(),
            pid: 12345,
            started_at: Utc::now(),
            memory_pressure: Some(0.75),  // v1.1 field
            cpu_usage: None,               // v1.1 field, not set
        };

        let json = serde_json::to_string(&session).unwrap();

        // memory_pressure should be present
        assert!(json.contains("memory_pressure"));

        // cpu_usage should be absent (skip_serializing_if = "Option::is_none")
        assert!(!json.contains("cpu_usage"));
    }

    #[test]
    fn test_v1_message_parsed_by_v1_1() {
        let v1_json = r#"{
            "session_id": "test-123",
            "status": "active",
            "agent_type": "claude-code",
            "pid": 12345
        }"#;

        // v1.1 parser should handle v1.0 message
        let session: Session = serde_json::from_str(v1_json).unwrap();

        assert_eq!(session.session_id, "test-123");
        assert!(session.memory_pressure.is_none());  // Not in v1.0
    }
}
```

### Integration Tests

```rust
#[tokio::test]
async fn test_version_negotiation_flow() {
    // Start test daemon
    let daemon = start_test_daemon().await;

    // Connect client
    let mut client = connect_to_daemon().await;

    // Send hello
    let hello = ClientHello {
        protocol_version: "1.0".to_string(),
        client_type: ClientType::Tui,
        capabilities: None,
    };
    client.send(&hello).await.unwrap();

    // Receive response
    let response: ServerHello = client.receive().await.unwrap();
    assert!(response.accepted);
    assert_eq!(response.protocol_version, "1.0");
}

#[tokio::test]
async fn test_status_update_optimization() {
    let daemon = start_test_daemon().await;
    let mut client = connect_to_daemon().await;

    // First message: includes version
    let register = ClientMessage {
        protocol_version: "1.0".to_string(),
        payload: ClientPayload::Register {
            session_id: "test-session".to_string(),
            agent_type: "claude-code".to_string(),
            pid: 12345,
        },
    };
    client.send(&register).await.unwrap();

    // Subsequent message: no version (optimization)
    let update_json = r#"{
        "type": "status_update",
        "session_id": "test-session",
        "context": {"used_percentage": 45.0}
    }"#;
    client.send_raw(update_json).await.unwrap();

    // Should still be processed correctly
    let sessions = client.get_all_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
}
```

---

## Summary

| Aspect | Specification |
|--------|---------------|
| Version Format | `MAJOR.MINOR` (e.g., `1.0`) |
| Current Version | `1.0` |
| Compatibility | Same major version = compatible |
| Negotiation | ClientHello / ServerHello handshake |
| Optimization | Version in first message only |
| Minor Changes | Add optional fields |
| Major Changes | Breaking changes, 6-month migration |
| Error Handling | User-friendly messages, structured logs |
