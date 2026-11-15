//! `session.kill` and `session.interrupt`: one actor turn each.
//!
//! The turn is the ordering. A kill or a Ctrl+C runs before or after a
//! reserved operation's capture, check and key, never between them. Neither
//! parks behind a reservation: they are how a caller stops something, and a
//! reservation can be held for minutes by the thing being stopped. Reserved
//! work already meets a pane that is gone or changed (`PaneGone`,
//! `ScreenChanged`, both before effect), since a pane can be closed by hand.
//!
//! An interrupt is input: it is refused while its pane is being cleared (the
//! Ctrl+C would wipe the clear command just typed, and a clear starts only at
//! a turn end) and while its pane is quarantined. A kill is never refused by
//! the reservation state: it sends no input and is the one remedy for a pane
//! the daemon no longer trusts. The organisation's record of a killed agent
//! follows from the agent observer, not from here.
//!
//! A request that expired in the queue, or whose caller is gone, touches
//! nothing and is no action. Once the single tmux call starts it runs under
//! the tmux client's own timeout. Audits follow [`legacy_input`]'s rule: for
//! a resolved pane, success or failure, through the non-blocking queue.
use super::*;
use crate::control::reservation::{state::InputGate, PaneId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    Kill,
    Interrupt,
}

impl Operation {
    fn method(self) -> &'static str {
        match self {
            Self::Kill => "session.kill",
            Self::Interrupt => "session.interrupt",
        }
    }

    /// The verb in a refusal: "Refusing to kill %7: ...".
    fn verb(self) -> &'static str {
        match self {
            Self::Kill => "kill",
            Self::Interrupt => "interrupt",
        }
    }
}

pub(super) fn is_pane_request(pending: &Pending) -> bool {
    matches!(
        pending.request,
        Request::Kill { .. } | Request::Interrupt { .. }
    )
}

/// Whether the reservation state of `pane` lets `operation` act in this turn.
/// There is no "wait": every answer is act or refuse.
pub(super) fn admit(
    operation: Operation,
    pane: &PaneId,
    gate: InputGate,
) -> Result<(), ServiceError> {
    match (operation, gate) {
        (Operation::Kill, _) | (Operation::Interrupt, InputGate::Free | InputGate::Held) => Ok(()),
        // A refusal string, as `canonical` gives: `ControlError::AgentClearing`
        // says "send".
        (Operation::Interrupt, InputGate::Clearing(agent)) => {
            Err(ServiceError::ServerRefused(format!(
                "Refusing to interrupt {pane}: agent {agent} is clearing; try again in a moment"
            )))
        }
        (Operation::Interrupt, InputGate::Quarantined(agent)) => {
            Err(ControlError::PaneQuarantined {
                pane: pane.to_string(),
                agent,
            }
            .into())
        }
        (Operation::Interrupt, InputGate::Unavailable) => Err(ServiceError::Unavailable),
    }
}

pub(super) async fn dispatch(
    client: &dyn TmuxClient,
    registry: &RegistryHandle,
    // Read only; `&Jobs` is not `Send` across the awaits here.
    jobs: &mut Jobs,
    pending: Pending,
    audits: &AuditQueue,
) {
    let Pending {
        server,
        deadline,
        caller,
        request,
    } = pending;
    let (operation, target, reply) = match request {
        Request::Kill { target, reply } => (Operation::Kill, target, reply),
        Request::Interrupt { target, reply } => (Operation::Interrupt, target, reply),
        _ => return,
    };
    if reply.is_closed() {
        return;
    }
    let resolved = outcome(deadline, async {
        // A foreign server is refused before it is an attributable action.
        if let Some(server) = server {
            let operation = format!("{} {target}", operation.verb());
            verify_server(client, server, operation).await?;
        }
        resolve(client, registry, operation, &target).await
    })
    .await;
    // Set once the request is an action: resolved, in time, its caller there.
    let mut acted_on = None;
    let result = match resolved {
        Err(error) => Err(error),
        Ok(_) if Instant::now() >= deadline => Err(ServiceError::Timeout),
        Ok(_) if reply.is_closed() => Err(ServiceError::Unavailable),
        Ok(pane) => {
            acted_on = Some(pane.to_string());
            match admit(operation, &pane, jobs.input_gate(&pane)) {
                Err(error) => Err(error),
                // The resolved pane is its own target: a `%` id needs no row.
                Ok(()) => match operation {
                    Operation::Kill => crate::control::kill(client, &[], pane.as_str()).await,
                    Operation::Interrupt => {
                        crate::control::interrupt(client, &[], pane.as_str()).await
                    }
                }
                .map_err(ServiceError::from),
            }
        }
    };
    let command = acted_on.as_deref().map(|pane| {
        let audit = control_audit(operation.method(), pane, caller.as_ref(), &result);
        match operation {
            Operation::Kill => OrgCommand::ControlKilled(audit),
            Operation::Interrupt => OrgCommand::ControlSent(audit),
        }
    });
    let _ = reply.send(result);
    if let Some(command) = command {
        audits.submit(command, chrono::Utc::now(), None);
    }
}

/// The registry's whole list, as the CLI's `ListSessions` read it before.
async fn resolve(
    client: &dyn TmuxClient,
    registry: &RegistryHandle,
    operation: Operation,
    target: &str,
) -> Result<PaneId, ServiceError> {
    if !registry.is_connected() {
        return Err(ServiceError::Unavailable);
    }
    let sessions = registry.get_all_sessions().await;
    if !registry.is_connected() {
        return Err(ServiceError::Unavailable);
    }
    let pane = crate::control::resolve_pane_id(&sessions, target)?;
    legacy_input::canonical(client, pane, operation.verb()).await
}
