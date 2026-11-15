//! Legacy-format capture and dialog input in the sole control actor. Pacing
//! yields, but a fresh numbered capture and its keys stay in one actor turn.
//! Read-only captures deliberately have no audit: waiters poll them frequently.
use super::org_mirror::Mirror;
use super::*;
use crate::control::{
    reply::{finish_reply, prepare_reply},
    reservation::PaneId,
    ReplyOutcome,
};

pub(super) fn is_request(pending: &Pending) -> bool {
    matches!(
        pending.request,
        Request::Reply { .. } | Request::Capture { .. }
    )
}
fn closed(pending: &Pending) -> bool {
    match &pending.request {
        Request::Reply { reply, .. } => reply.is_closed(),
        Request::Capture { reply, .. } => reply.is_closed(),
        _ => true,
    }
}

/// The existing scheduler owns this while waiting. It releases it back to the
/// actor so registry, mirror and reservation admission are fresh after pacing.
pub(in crate::control) struct Deferred {
    pub(in crate::control) pane: PaneId,
    pending: Pending,
    audits: AuditQueue,
}
impl Deferred {
    pub(in crate::control) fn deadline(&self) -> Instant {
        self.pending.deadline
    }
    pub(super) fn ready(self) -> (Pending, Option<PaneId>) {
        (self.pending, Some(self.pane))
    }
    pub(in crate::control) fn reject(self, error: ReservationError) {
        let error = match error {
            ReservationError::Timeout => ServiceError::Timeout,
            ReservationError::Unavailable => ServiceError::Unavailable,
            error => ServiceError::ServerRefused(error.to_string()),
        };
        reject(self.pending, error, Some(&self.pane), &self.audits);
    }
}

fn reject(pending: Pending, error: ServiceError, pane: Option<&PaneId>, audits: &AuditQueue) {
    let live = !closed(&pending) && Instant::now() < pending.deadline;
    match pending.request {
        Request::Reply { reply, .. } => {
            let result: Result<ReplyOutcome, ServiceError> = Err(error);
            let audit = pane.filter(|_| live).map(|pane| {
                OrgCommand::ControlSent(control_audit(
                    "session.reply",
                    pane.as_str(),
                    pending.caller.as_ref(),
                    &result,
                ))
            });
            let _ = reply.send(result);
            if let Some(audit) = audit {
                audits.submit(audit, chrono::Utc::now(), None);
            }
        }
        Request::Capture { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        _ => {}
    }
}

/// None is completed or paced; Some parks the original target/deadline. A pace
/// grant belongs to a canonical pane, never to an alias that may have changed.
pub(super) async fn dispatch(
    client: &dyn TmuxClient,
    registry: &RegistryHandle,
    jobs: &mut Jobs,
    mirror: &Mirror,
    (mut pending, granted): (Pending, Option<PaneId>),
    audits: &AuditQueue,
    may_park: bool,
) -> Option<Pending> {
    if closed(&pending) {
        return None;
    }
    let (target, verb, captures) = match &pending.request {
        Request::Reply {
            target,
            option,
            yes,
            no,
            ..
        } => (target.clone(), "reply to", option.is_some() && !yes && !no),
        Request::Capture { target, .. } => (target.clone(), "capture", true),
        _ => return None,
    };
    if let Some(server) = pending.server.take() {
        if let Err(error) = outcome(
            pending.deadline,
            verify_server(client, server, format!("{verb} {target}")),
        )
        .await
        {
            reject(pending, error, None, audits);
            return None;
        }
    }
    let pane = match outcome(
        pending.deadline,
        pane_request::resolve(client, registry, verb, &target),
    )
    .await
    {
        Ok(pane) => pane,
        Err(error) => {
            reject(pending, error, None, audits);
            return None;
        }
    };
    // Do not retain shared references to Pending/Jobs across this read: their
    // daemon-owned jobs/guards are Send, not Sync.
    let input = matches!(pending.request, Request::Reply { .. });
    let gate = jobs.input_gate(&pane);
    let admitted = outcome(pending.deadline, async {
        if input {
            legacy_input::admit_input(client, &pane, gate, &mirror.view()).await.map_err(|error| match error {
                ServiceError::Control(ControlError::AgentClearing { pane, agent }) => ServiceError::ServerRefused(format!("Refusing to reply to {pane}: agent {agent} is clearing; try again in a moment")),
                error => error,
            })
        } else { Ok(true) }
    }).await;
    if closed(&pending) {
        return None;
    }
    if Instant::now() >= pending.deadline {
        reject(pending, ServiceError::Timeout, None, audits);
        return None;
    }
    match admitted {
        Ok(false) if may_park => return Some(pending),
        Ok(false) => {
            reject(pending, ServiceError::Unavailable, Some(&pane), audits);
            return None;
        }
        Err(error) => {
            reject(pending, error, Some(&pane), audits);
            return None;
        }
        Ok(true) => {}
    }
    if captures && granted.as_ref() != Some(&pane) {
        jobs.dispatch(
            Command::LegacyCapture(Box::new(Deferred {
                pane,
                pending,
                audits: audits.clone(),
            })),
            Instant::now(),
            false,
            client,
        )
        .await;
        return None;
    }
    let Pending {
        deadline,
        caller,
        request,
        ..
    } = pending;
    match request {
        Request::Capture {
            tail,
            prompt,
            reply,
            ..
        } => {
            let result = outcome(deadline, async {
                Ok(crate::control::peek(client, &[], pane.as_str(), tail, prompt).await?)
            })
            .await;
            let _ = reply.send(result);
        }
        Request::Reply {
            option,
            yes,
            no,
            reply,
            ..
        } => {
            let prepared = outcome(deadline, async {
                Ok(prepare_reply(client, pane.to_string(), option, yes, no).await?)
            })
            .await;
            let mut started = false;
            let result = match prepared {
                Err(error) => Err(error),
                Ok(prepared) => {
                    outcome(
                        deadline,
                        finish_reply(client, prepared, || {
                            if Instant::now() >= deadline {
                                return Err(ServiceError::Timeout);
                            }
                            if !started && reply.is_closed() {
                                return Err(ServiceError::Unavailable);
                            }
                            started = true;
                            Ok(())
                        }),
                    )
                    .await
                }
            };
            // Dropped callers never admit input, including after a capture. Once
            // a key starts, retain its audit even on disconnect or total timeout.
            let audit = (started || (!reply.is_closed() && Instant::now() < deadline)).then(|| {
                OrgCommand::ControlSent(control_audit(
                    "session.reply",
                    pane.as_str(),
                    caller.as_ref(),
                    &result,
                ))
            });
            let _ = reply.send(result);
            if let Some(audit) = audit {
                audits.submit(audit, chrono::Utc::now(), None);
            }
        }
        _ => {}
    }
    None
}
