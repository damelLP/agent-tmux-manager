//! Named-agent service contract. No lifecycle effects are enabled yet.
//!
//! A disabled handle reports its cause; the runtime checkpoint reports typed
//! NotImplemented. Neither reads files, sends pane input nor changes organisation
//! state. Later startup wiring must reuse the daemon's single control handle.

pub use super::agent_methods::register_agent_methods;
pub use super::agent_types::*;
use super::{OrgHandle, OrgServiceError};
use crate::server::methods::CallContext;
use crate::{control::service::ControlHandle, registry::RegistryHandle};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent operation '{operation}' is not implemented")]
    NotImplemented { operation: &'static str },
    #[error("invalid agent parameters: {0}")]
    InvalidParams(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Refused(String),
    #[error("agent operation timed out; admitted effects may have completed; inspect agent list")]
    Timeout,
    #[error("agent service unavailable: {0}")]
    Unavailable(String),
    #[error("agent operation failed: {0}")]
    Internal(String),
    #[error(transparent)]
    Reservation(#[from] crate::control::reservation::ReservationError),
    #[error(transparent)]
    Organisation(#[from] OrgServiceError),
}

#[derive(Debug)]
struct AgentService {
    disabled: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentHandle {
    inner: Arc<AgentService>,
}

impl AgentHandle {
    #[must_use]
    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(AgentService {
                disabled: Some(reason.into()),
            }),
        }
    }

    fn unavailable(&self, operation: &'static str) -> AgentError {
        match &self.inner.disabled {
            Some(reason) => AgentError::Unavailable(reason.clone()),
            None => AgentError::NotImplemented { operation },
        }
    }

    pub async fn up(&self, _p: UpParams, _caller: CallContext) -> Result<UpResult, AgentError> {
        Err(self.unavailable("agent.up"))
    }
    pub async fn adopt(&self, _p: AdoptParams) -> Result<AdoptResult, AgentError> {
        Err(self.unavailable("agent.adopt"))
    }
    pub async fn clear(&self, _p: ClearParams) -> Result<ClearResult, AgentError> {
        Err(self.unavailable("agent.clear"))
    }
    pub async fn respawn(
        &self,
        _p: RespawnParams,
        _caller: CallContext,
    ) -> Result<RespawnResult, AgentError> {
        Err(self.unavailable("agent.respawn"))
    }
    pub async fn down(&self, _p: DownParams) -> Result<DownResult, AgentError> {
        Err(self.unavailable("agent.down"))
    }
    pub async fn list(&self, _p: ListParams) -> Result<ListResult, AgentError> {
        Err(self.unavailable("agent.list"))
    }
    pub async fn reconciled(&self) -> Result<(), AgentError> {
        Err(self.unavailable("reconciled"))
    }
}

/// Compile-ready runtime seam. The checkpoint task completes without effects;
/// daemon startup intentionally does not install it until reconciliation exists.
#[must_use]
pub fn spawn_agents(
    _org: OrgHandle,
    _registry: RegistryHandle,
    _control: ControlHandle,
    _cancel: CancellationToken,
) -> (AgentHandle, JoinHandle<()>) {
    (
        AgentHandle {
            inner: Arc::new(AgentService { disabled: None }),
        },
        tokio::spawn(async {}),
    )
}
