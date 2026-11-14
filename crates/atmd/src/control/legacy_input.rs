//! Legacy input joins the same actor ordering as reserved pane effects.
use super::org_mirror::{Mirror, View};
use super::*;
use crate::control::reservation::{pane_ops, state::InputGate, PaneId};
use atm_core::SessionView;
use atm_org::AgentStatus;

struct Prepared {
    pane: PaneId,
    sessions: Vec<SessionView>,
}

pub(super) fn is_input(pending: &Pending) -> bool {
    matches!(pending.request, Request::Send { .. } | Request::Type { .. })
}

/// None means completion; Some retains the original target for re-resolution.
/// The caller bounds this parked queue and wakes it on actor/mirror progress.
pub(super) async fn dispatch(
    client: &dyn TmuxClient,
    registry: &RegistryHandle,
    jobs: &mut Jobs,
    mirror: &Mirror,
    mut pending: Pending,
    audits: &AuditQueue,
    may_park: bool,
) -> Option<Pending> {
    // Preserve U7's identity boundary: a foreign server is rejected before it
    // becomes an attributable control action, including before any ownership I/O.
    if let Some(server) = pending.server.take() {
        let operation = match &pending.request {
            Request::Send { target, .. } => format!("send to {target}"),
            Request::Type { pane, .. } => format!("spawn in {pane}"),
            _ => return None,
        };
        if let Err(error) =
            outcome(pending.deadline, verify_server(client, server, operation)).await
        {
            match pending.request {
                Request::Send { reply, .. } => {
                    let _ = reply.send(Err(error));
                }
                Request::Type { reply, .. } => {
                    let _ = reply.send(Err(error));
                }
                _ => {}
            }
            return None;
        }
    }
    let mut pane = match &pending.request {
        Request::Send { target, .. } => target.starts_with('%').then(|| target.clone()),
        Request::Type { pane, .. } => Some(pane.clone()),
        _ => None,
    };
    let prepared = outcome(
        pending.deadline,
        prepare(client, registry, jobs, mirror, &mut pending, &mut pane),
    )
    .await;
    let prepared = match prepared {
        Ok(None) if may_park => return Some(pending),
        Ok(None) => Err(ServiceError::Unavailable),
        Ok(Some(prepared)) => Ok(prepared),
        Err(error) => Err(error),
    };
    let expired = Instant::now() >= pending.deadline;
    let caller = pending.caller.as_ref();
    let command = match pending.request {
        Request::Send {
            target,
            text,
            reply,
        } => {
            let result = outcome(pending.deadline, async {
                let ready = prepared?;
                Ok(crate::control::send::send_resolved(
                    client,
                    &ready.sessions,
                    &target,
                    ready.pane.as_str(),
                    &text,
                )
                .await?)
            })
            .await;
            respond(reply, result, |result| {
                pane.as_deref().map(|pane| {
                    OrgCommand::ControlSent(control_audit("session.send", pane, caller, result))
                })
            })
        }
        Request::Type { command, reply, .. } => {
            let result = outcome(pending.deadline, async {
                let ready = prepared?;
                Ok(
                    crate::control::type_spawn_command(client, ready.pane.as_str(), &command)
                        .await?,
                )
            })
            .await;
            respond(reply, result, |result| {
                pane.as_deref().map(|pane| {
                    OrgCommand::ControlSent(control_audit(
                        "session.spawn_type",
                        pane,
                        caller,
                        result,
                    ))
                })
            })
        }
        _ => None,
    };
    if !expired {
        if let Some(command) = command {
            audits.submit(command, chrono::Utc::now(), None);
        }
    }
    None
}

async fn canonical(client: &dyn TmuxClient, target: String) -> Result<PaneId, ServiceError> {
    if let Ok(pane) = PaneId::parse(target.clone()) {
        return Ok(pane);
    }
    let resolved = client
        .raw_command("display-message", &["-p", "-t", &target, "#{pane_id}"])
        .await
        .map_err(|source| ControlError::Tmux {
            context: format!("Failed to resolve pane {target}"),
            source,
        })?;
    PaneId::parse(resolved.trim_end_matches('\n')).map_err(|_| {
        ServiceError::ServerRefused(format!(
            "Refusing to send to {target}: cannot resolve a canonical pane"
        ))
    })
}

async fn prepare(
    client: &dyn TmuxClient,
    registry: &RegistryHandle,
    jobs: &mut Jobs,
    mirror: &Mirror,
    pending: &mut Pending,
    audit_pane: &mut Option<String>,
) -> Result<Option<Prepared>, ServiceError> {
    let mut sessions = Vec::new();
    let target = match &pending.request {
        Request::Send { target, .. } => {
            if !registry.is_connected() {
                return Err(ServiceError::Unavailable);
            }
            sessions = registry.get_all_sessions().await;
            if !registry.is_connected() {
                return Err(ServiceError::Unavailable);
            }
            crate::control::resolve_pane_id(&sessions, target)?
        }
        Request::Type { pane, .. } => pane.clone(),
        _ => return Err(ServiceError::Unavailable),
    };
    let pane = canonical(client, target).await?;
    *audit_pane = Some(pane.to_string());
    let held = match jobs.input_gate(&pane) {
        InputGate::Free => false,
        InputGate::Held => true,
        InputGate::Clearing(agent) => {
            return Err(ControlError::AgentClearing {
                pane: pane.to_string(),
                agent,
            }
            .into())
        }
        InputGate::Quarantined(agent) => {
            return Err(ControlError::PaneQuarantined {
                pane: pane.to_string(),
                agent,
            }
            .into())
        }
        InputGate::Unavailable => return Err(ServiceError::Unavailable),
    };
    match mirror.view() {
        View::Loading => return Ok(None),
        View::Unavailable if held => return Ok(None),
        View::Unavailable => {
            if !matches!(pane_ops::inspect(client, &pane).await, Ok(Some(info)) if info.local_agent_tag.is_none())
            {
                return Err(ControlError::OrganisationUnavailable {
                    pane: pane.to_string(),
                }
                .into());
            }
        }
        View::Ready(state) => {
            if let Some(agent) = state
                .agent_for_pane(pane.as_str())
                .filter(|agent| agent.status == AgentStatus::Clearing)
            {
                return Err(ControlError::AgentClearing {
                    pane: pane.to_string(),
                    agent: agent.id.clone(),
                }
                .into());
            }
            if held {
                return Ok(None);
            }
            sessions.retain(|row| {
                !row.tmux_pane
                    .as_deref()
                    .and_then(|pane| state.agent_for_pane(pane))
                    .is_some_and(|owner| {
                        owner
                            .excluded_session_id
                            .as_ref()
                            .is_some_and(|id| id.as_str() == row.id.as_str())
                    })
            });
            // Resolve again only after exclusion; never redirect an obsolete prefix.
            if let Request::Send { target, .. } = &pending.request {
                crate::control::resolve_pane_id(&sessions, target)?;
            }
        }
    }
    Ok(Some(Prepared { pane, sessions }))
}
