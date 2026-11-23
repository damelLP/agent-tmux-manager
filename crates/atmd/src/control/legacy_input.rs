//! Legacy input joins the same actor ordering as reserved pane effects.
//! Parked input yields to reservation waiters on its pane, including later
//! arrivals. A steady stream of reservations can starve it until its deadline.
//! A send whose caller is gone before the first effect is discarded, including
//! after pre-effect reads. Once input starts, finish Enter under tmux's timeout.
//! Spawn typing retains daemon-owned admission even if its caller disappears.
use super::org_mirror::{Mirror, View};
use super::*;
use crate::control::reservation::{pane_ops, state::InputGate, CaptureFormat, PaneCapture, PaneId};
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
    if matches!(&pending.request, Request::Send { reply, .. } if reply.is_closed()) {
        return None;
    }
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
            let result = async {
                let ready = outcome(pending.deadline, async {
                    let ready = prepared?;
                    crate::control::send::check_send(
                        client,
                        &ready.sessions,
                        &target,
                        ready.pane.as_str(),
                        &text,
                    )
                    .await?;
                    if reply.is_closed() {
                        return Err(ServiceError::Unavailable);
                    }
                    guard_legacy_input(client, &ready.pane, "send to", |capture| {
                        crate::org::agent_screen::legacy_send_veto(capture, &text)
                    })
                    .await?;
                    Ok(ready)
                })
                .await?;
                if Instant::now() >= pending.deadline {
                    return Err(ServiceError::Timeout);
                }
                if reply.is_closed() {
                    return Err(ServiceError::Unavailable);
                }
                // Admission expires before the first effect, never between paste
                // and Enter. Each tmux call retains its own bounded timeout.
                Ok(
                    crate::control::send::paste_and_submit(client, ready.pane.as_str(), &text)
                        .await?,
                )
            }
            .await;
            respond(reply, result, |result| {
                pane.as_deref().map(|pane| {
                    OrgCommand::ControlSent(control_audit("session.send", pane, caller, result))
                })
            })
        }
        Request::Type { command, reply, .. } => {
            let result = async {
                let ready = prepared?;
                if Instant::now() >= pending.deadline {
                    return Err(ServiceError::Timeout);
                }
                Ok(
                    crate::control::type_spawn_command(client, ready.pane.as_str(), &command)
                        .await?,
                )
            }
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

/// A fresh veto in the actor turn, after admission and before the first effect.
pub(super) async fn guard_legacy_input(
    client: &dyn TmuxClient,
    pane: &PaneId,
    operation: &str,
    veto: impl FnOnce(&PaneCapture) -> Option<&'static str>,
) -> Result<(), ServiceError> {
    let refusal = |reason| {
        ServiceError::ServerRefused(format!(
            "Refusing to {operation} {pane}: {reason}; nothing was sent"
        ))
    };
    let capture = pane_ops::capture(client, pane, CaptureFormat::Ansi)
        .await
        .map_err(|error| match error {
            ReservationError::Control(error) => ServiceError::Control(error),
            other => refusal(other.to_string()),
        })?;
    if let Some(reason) = veto(&capture) {
        return Err(refusal(reason.to_owned()));
    }
    Ok(())
}

/// `operation` is the refusal's verb phrase: "send to", "kill", "interrupt".
pub(super) async fn canonical(
    client: &dyn TmuxClient,
    target: String,
    operation: &str,
) -> Result<PaneId, ServiceError> {
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
            "Refusing to {operation} {target}: cannot resolve a canonical pane"
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
    let pane = canonical(client, target, "send to").await?;
    *audit_pane = Some(pane.to_string());
    let view = mirror.view();
    if !admit_input(client, &pane, jobs.input_gate(&pane), &view).await? {
        return Ok(None);
    }
    if let View::Ready(state) = view {
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
    Ok(Some(Prepared { pane, sessions }))
}

/// Shared reservation/mirror admission: true acts, false parks. No snapshot or
/// persistence I/O runs here; unavailable organisation state fails closed unless
/// a fresh pane inspection positively establishes that the pane is untagged.
pub(super) async fn admit_input(
    client: &dyn TmuxClient,
    pane: &PaneId,
    gate: InputGate,
    view: &View,
) -> Result<bool, ServiceError> {
    let held = match gate {
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
    match view {
        View::Loading => return Ok(false),
        View::Unavailable if held => return Ok(false),
        View::Unavailable => {
            if !matches!(pane_ops::inspect(client, pane).await, Ok(Some(info)) if info.local_agent_tag.is_none())
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
                return Ok(false);
            }
        }
    }
    Ok(true)
}
