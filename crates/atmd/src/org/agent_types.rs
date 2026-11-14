//! Method-owned schemas carried by the existing protocol 1.1 JSON envelope.

use atm_core::{SessionId, SessionStatus};
pub use atm_org::{AgentId, AgentStatus};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpParams {
    pub project_root: PathBuf,
    pub name: String,
    /// Caller tmux socket path; absence retains legacy server selection.
    pub caller_server: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptParams {
    pub project_root: PathBuf,
    pub name: String,
    pub pane: String,
    /// Caller tmux socket path; absence retains legacy server selection.
    pub caller_server: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClearParams {
    pub agent: AgentId,
    #[serde(default = "default_clear_timeout_ms")]
    pub timeout_ms: u64,
}
#[must_use]
pub const fn default_clear_timeout_ms() -> u64 {
    30_000
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RespawnParams {
    pub agent: AgentId,
    /// Caller tmux socket path; absence retains legacy server selection.
    pub caller_server: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownParams {
    pub agent: AgentId,
    #[serde(default)]
    pub kill: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub project_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpResult {
    pub agent_id: AgentId,
    pub pane: String,
    pub state: AgentStatus,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptResult {
    pub agent_id: AgentId,
    pub state: AgentStatus,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearResult {
    pub agent_id: AgentId,
    pub session_id: Option<SessionId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RespawnResult {
    pub agent_id: AgentId,
    pub pane: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownResult {
    pub agent_id: AgentId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListEntry {
    pub agent_id: AgentId,
    pub state: AgentStatus,
    pub activity: Option<SessionStatus>,
    pub pane: Option<String>,
    pub session_id: Option<SessionId>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
    pub reason: Option<String>,
}
pub type ListResult = Vec<AgentListEntry>;
