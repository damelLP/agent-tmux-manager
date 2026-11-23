//! Connection-scoped routing and the single deadline runner. The actor commits
//! the answer before any wake. U5 repair completes a wake prefix after a failure
//! or dropped request; on replay the timer only sees posts without an answer.

use super::{
    channel_post::{owner_heals, wake_rows},
    channels::{json_error, org_error},
    message_inbox::{caller_address, INBOX_PAGE_BYTES},
    messages::{MessageClock, MessageError},
    OrgHandle, OrgServiceError,
};
use crate::{
    eventlog::Envelope,
    server::methods::{CallContext, ConnectionScope, MethodError},
};
use atm_comms::channel_types::{
    ChannelRouteParams, ChannelRouteResult, PendingRoute, RouterAttachParams, RouterAttachResult,
    WokeRow,
};
use atm_org::{
    channel_policy::{route_step, stopgap_wake, RouteStep, RouterPresence, RouterSettings},
    channel_query::{pending_routes, post, wake_for},
    Address, ApplyError, ChannelCommand, ChannelRefusal, MessageId, OrgCommand, OrgState,
    PostRouted, Refusal, RouteBy,
};
use atm_protocol::error_code;
use chrono::{DateTime, Utc};
use std::{collections::BTreeSet, future::pending, sync::Arc, time::Duration};
use tokio::{
    sync::{broadcast, Mutex, Notify},
    task::JoinHandle,
    time::{sleep_until, Instant},
};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "channel_route_tests.rs"]
mod tests;

#[derive(Clone)]
pub(crate) struct RouteHandle(Arc<Routing>);

struct Routing {
    org: OrgHandle,
    settings: RouterSettings,
    clock: MessageClock,
    // Hold through the eligibility check and Route submission, not its wakes.
    presence: Mutex<Presence>,
    changed: Notify,
}

enum Presence {
    Attached {
        scope: ConnectionScope,
        since: DateTime<Utc>,
        by: RouteBy,
    },
    Grace {
        until: DateTime<Utc>,
    },
    Absent,
}

impl Presence {
    fn refresh(
        &mut self,
        routing: &Routing,
        now: DateTime<Utc>,
    ) -> Result<RouterPresence, MessageError> {
        if let Self::Attached { scope, .. } = self {
            if let Some(closed) = scope.closed_at() {
                *self = Self::Grace {
                    until: timeout_from(&routing.settings, routing.clock.at(closed)?),
                };
            }
        }
        Ok(match self {
            Self::Attached { since, .. } => RouterPresence::Attached { since: *since },
            Self::Grace { until } if now < *until => RouterPresence::Grace { until: *until },
            _ => {
                *self = Self::Absent;
                RouterPresence::Absent
            }
        })
    }
}

struct TimerWait {
    seq: u64,
    until: Option<Instant>,
    attached: Option<ConnectionScope>,
}

enum TimerPass {
    Park(TimerWait),
    Rescan(Option<MessageId>),
}

impl RouteHandle {
    pub(crate) fn new(
        org: OrgHandle,
        settings: RouterSettings,
        clock: MessageClock,
        started: DateTime<Utc>,
    ) -> Self {
        let presence = if settings.enabled {
            Presence::Grace {
                until: timeout_from(&settings, started),
            }
        } else {
            Presence::Absent
        };
        Self(Arc::new(Routing {
            org,
            settings,
            clock,
            presence: Mutex::new(presence),
            changed: Notify::new(),
        }))
    }

    pub(crate) fn spawn(&self, stop: CancellationToken) -> JoinHandle<()> {
        let service = self.clone();
        tokio::spawn(async move { service.run(stop, |_, _| {}).await })
    }

    pub(super) async fn attach(
        &self,
        caller: &CallContext,
        scope: ConnectionScope,
        p: RouterAttachParams,
    ) -> Result<RouterAttachResult, MethodError> {
        if !self.0.settings.enabled {
            return Err(MethodError::new(error_code::REFUSED, "routing is disabled"));
        }
        // Both fields are copied into every decision by this attachment.
        for (field, value) in [("name", &p.name), ("model", &p.model)] {
            if value.is_empty()
                || value.len() > 128
                || value.contains([
                    '\n', '\r', '\u{000B}', '\u{000C}', '\u{0085}', '\u{2028}', '\u{2029}',
                ])
            {
                return Err(MethodError::new(
                    error_code::INVALID_PARAMS,
                    format!("router {field} must be one line of 1 to 128 bytes"),
                ));
            }
        }
        let mut presence = self.0.presence.lock().await;
        let (state, seq) = self.0.org.snapshot_at().await.map_err(org_error)?;
        owner(&state, caller)?;
        let since = self.0.clock.now()?;
        if matches!(
            presence.refresh(&self.0, since)?,
            RouterPresence::Attached { .. }
        ) {
            return Err(MethodError::new(
                error_code::REFUSED,
                "a router is already attached",
            ));
        }
        let pending = pending_routes(&state)
            .filter_map(|(id, at)| {
                Some(PendingRoute {
                    post: id,
                    channel: post(&state, id)?.content.channel.clone(),
                    deadline: attached_deadline(at, since, &self.0.settings),
                })
            })
            .collect();
        let result = RouterAttachResult {
            seq,
            timeout_secs: self.0.settings.timeout_secs,
            pending,
        };
        if serde_json::to_vec(&result).map_err(json_error)?.len() > INBOX_PAGE_BYTES {
            return Err(MethodError::new(
                error_code::REFUSED,
                "router.attach result exceeds the 512 KiB reply limit",
            ));
        }
        require_open(&scope)?;
        *presence = Presence::Attached {
            scope,
            since,
            by: RouteBy::Router {
                name: p.name,
                model: Some(p.model),
            },
        };
        self.0.changed.notify_one();
        Ok(result)
    }

    pub(super) async fn answer(
        &self,
        caller: &CallContext,
        scope: ConnectionScope,
        p: ChannelRouteParams,
    ) -> Result<ChannelRouteResult, MethodError> {
        let id = match &p {
            ChannelRouteParams::Wake { post, .. } | ChannelRouteParams::Decline { post, .. } => {
                *post
            }
        };
        let changed = {
            let mut presence = self.0.presence.lock().await;
            let state = self.0.org.snapshot().await.map_err(org_error)?;
            owner(&state, caller)?;
            let now = self.0.clock.now()?;
            let policy = presence.refresh(&self.0, now)?;
            let Presence::Attached {
                scope: attached,
                by,
                ..
            } = &*presence
            else {
                return Err(not_attached());
            };
            if !attached.same_connection(&scope) {
                return Err(not_attached());
            }
            let row = post(&state, id).ok_or_else(|| {
                MethodError::new(error_code::INVALID_PARAMS, format!("unknown post {id}"))
            })?;
            if !row.content.route {
                return Err(MethodError::new(
                    error_code::INVALID_PARAMS,
                    format!("post {id} was not waiting for routing"),
                ));
            }
            require_open(&scope)?;
            if row.routed.is_none()
                && matches!(
                    route_step(row.posted_at, now, &self.0.settings, policy),
                    RouteStep::Stopgap(_)
                )
            {
                return Err(MethodError::new(
                    error_code::CONFLICT,
                    format!("deadline for post {id} has passed"),
                ));
            }
            match p {
                ChannelRouteParams::Wake { wake, reason, .. } => {
                    if reason.as_ref().is_some_and(|r| r.contains(['\r', '\n'])) {
                        return Err(MethodError::new(
                            error_code::INVALID_PARAMS,
                            "reason must be one line",
                        ));
                    }
                    self.commit(
                        PostRouted {
                            post: id,
                            wake,
                            by: by.clone(),
                            reason,
                        },
                        now,
                    )
                    .await
                    .map_err(route_error)?
                }
                ChannelRouteParams::Decline { .. } => match &row.routed {
                    Some(first) if matches!(&first.by, RouteBy::Stopgap { cause } if cause == "declined") => {
                        false
                    }
                    Some(_) => {
                        return Err(MethodError::new(
                            error_code::CONFLICT,
                            format!("post {id} is already routed"),
                        ))
                    }
                    None => self
                        .commit_stopgap(&state, id, now, "declined")
                        .await
                        .map_err(route_error)?,
                },
            }
        };
        if changed {
            self.write_wakes(id).await?;
        }
        let (state, seq) = self.0.org.snapshot_at().await.map_err(org_error)?;
        Ok(route_result(&state, seq, id))
    }

    async fn commit(
        &self,
        answer: PostRouted,
        now: DateTime<Utc>,
    ) -> Result<bool, OrgServiceError> {
        self.0
            .org
            .execute(OrgCommand::Channel(ChannelCommand::Route(answer)), now)
            .await
            .map(|events| !events.is_empty())
    }

    async fn commit_stopgap(
        &self,
        state: &OrgState,
        post: MessageId,
        now: DateTime<Utc>,
        cause: &'static str,
    ) -> Result<bool, OrgServiceError> {
        let answer = |wake| PostRouted {
            post,
            wake,
            by: RouteBy::Stopgap {
                cause: cause.into(),
            },
            reason: None,
        };
        match self.commit(answer(stopgap_wake(state, post)), now).await {
            Err(OrgServiceError::Refused(refusal)) if !already_routed(&refusal) => {
                tracing::error!(%post, %refusal, "Stopgap answer refused; recording an empty answer");
                self.commit(answer(Vec::new()), now).await
            }
            result => result,
        }
    }

    async fn write_wakes(&self, post: MessageId) -> Result<(), MessageError> {
        let org = &self.0.org;
        let rows = wake_rows(&org.snapshot().await?, Some(post));
        for row in &rows {
            org.execute(OrgCommand::SendMessage(row.clone()), self.0.clock.now()?)
                .await?;
        }
        if !rows.is_empty() {
            for heal in owner_heals(&org.snapshot().await?, &rows) {
                org.execute(heal, self.0.clock.now()?).await?;
            }
        }
        Ok(())
    }

    async fn pass(&self, skip: &mut BTreeSet<MessageId>) -> Result<TimerPass, MessageError> {
        let mut presence = self.0.presence.lock().await;
        let (state, seq) = self.0.org.snapshot_at().await?;
        let now = self.0.clock.now()?;
        let policy = presence.refresh(&self.0, now)?;
        let mut first = DateTime::<Utc>::MAX_UTC;
        for (id, at) in pending_routes(&state) {
            if skip.contains(&id) {
                continue;
            }
            match route_step(at, now, &self.0.settings, policy) {
                RouteStep::Wait(until) => first = first.min(until),
                RouteStep::Stopgap(cause) => {
                    let routed = match self.commit_stopgap(&state, id, now, cause).await {
                        Ok(changed) => changed,
                        Err(OrgServiceError::Refused(refusal)) => {
                            if !already_routed(&refusal) {
                                tracing::error!(post = %id, %refusal, "Empty stopgap answer refused; skipping until restart");
                                skip.insert(id);
                            }
                            false
                        }
                        Err(error) => return Err(error.into()),
                    };
                    return Ok(TimerPass::Rescan(routed.then_some(id)));
                }
            }
        }
        let attached = match &*presence {
            Presence::Attached { scope, .. } => Some(scope.clone()),
            _ => None,
        };
        let until = if first == DateTime::<Utc>::MAX_UTC {
            None
        } else {
            self.0.clock.deadline(first).ok()
        };
        Ok(TimerPass::Park(TimerWait {
            seq,
            until,
            attached,
        }))
    }

    async fn run(
        &self,
        stop: CancellationToken,
        mut on_park: impl FnMut(u64, Option<Instant>) + Send,
    ) {
        let org = &self.0.org;
        // Notifications invalidate a snapshot, not a second fold. Lag does
        // not require replay: the next snapshot includes every applied post.
        let mut records = org.committed.subscribe();
        let mut health = org.health.clone();
        let mut skip = BTreeSet::new();
        let work = async {
            loop {
                match self.pass(&mut skip).await {
                    Ok(TimerPass::Rescan(post)) => {
                        if let Some(post) = post {
                            if let Err(error) = self.write_wakes(post).await {
                                tracing::warn!(%post, %error, "Route saved; remaining wakes left for repair");
                            }
                        }
                    }
                    Ok(TimerPass::Park(TimerWait {
                        seq,
                        until,
                        mut attached,
                    })) => {
                        on_park(seq, until);
                        tokio::select! {
                            () = next_route_change(&mut records, seq) => {},
                            () = self.0.changed.notified() => {},
                            () = wait_until(until) => {},
                            () = wait_closed(attached.as_mut()) => {},
                        }
                    }
                    Err(MessageError::Organisation(
                        error @ (OrgServiceError::Timeout | OrgServiceError::Unavailable(_)),
                    )) if org.status().is_ok() && !org.requests.is_closed() => {
                        tracing::warn!(%error, "Routing pass incomplete; retrying");
                        let retry = Instant::now() + Duration::from_secs(1);
                        on_park(org.status()?, Some(retry));
                        sleep_until(retry).await;
                    }
                    Err(error) => return Err::<(), _>(error),
                }
            }
        };
        let result = tokio::select! {
            biased;
            () = stop.cancelled() => Ok(()),
            _ = health.wait_for(Result::is_err) => Err(MessageError::Unavailable("organisation writer stopped".into())),
            result = work => result,
        };
        if let Err(error) = result {
            tracing::warn!(%error, "Channel routing stopped; restart to recover");
        }
    }
}

fn attached_deadline(
    at: DateTime<Utc>,
    since: DateTime<Utc>,
    settings: &RouterSettings,
) -> DateTime<Utc> {
    match route_step(at, since, settings, RouterPresence::Attached { since }) {
        RouteStep::Wait(until) => until,
        RouteStep::Stopgap(_) => since,
    }
}

fn timeout_from(settings: &RouterSettings, start: DateTime<Utc>) -> DateTime<Utc> {
    attached_deadline(start, start, settings)
}

async fn wait_until(at: Option<Instant>) {
    if let Some(at) = at {
        sleep_until(at).await;
    } else {
        pending().await
    }
}

async fn wait_closed(scope: Option<&mut ConnectionScope>) {
    if let Some(scope) = scope {
        scope.wait_closed().await;
    } else {
        pending().await
    }
}

async fn next_route_change(records: &mut broadcast::Receiver<Arc<Envelope>>, seq: u64) {
    while let Ok(record) = records.recv().await {
        if record.seq > seq
            && (record.kind == "channel.routed"
                || (record.kind == "channel.posted"
                    && record
                        .body
                        .get("route")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)))
        {
            break;
        }
    }
}

fn owner(state: &OrgState, caller: &CallContext) -> Result<(), MethodError> {
    if caller_address(state, caller)? != Address::Owner {
        return Err(MethodError::new(
            error_code::REFUSED,
            "routing requires owner scope",
        ));
    }
    Ok(())
}

fn not_attached() -> MethodError {
    MethodError::new(
        error_code::REFUSED,
        "this connection is not the attached router",
    )
}

fn require_open(scope: &ConnectionScope) -> Result<(), MethodError> {
    if scope.closed_at().is_some() {
        return Err(not_attached());
    }
    Ok(())
}

fn already_routed(refusal: &Refusal) -> bool {
    matches!(refusal, Refusal::Channel(ChannelRefusal::AlreadyRouted(_)))
}

fn route_error(error: OrgServiceError) -> MethodError {
    match &error {
        OrgServiceError::Refused(r) if already_routed(r) => {
            MethodError::new(error_code::CONFLICT, error.to_string())
        }
        OrgServiceError::Refused(
            Refusal::Invariant(ApplyError::UnknownPost(_))
            | Refusal::InvalidValue { .. }
            | Refusal::Channel(ChannelRefusal::NotRouted(_) | ChannelRefusal::NotRoutable { .. }),
        ) => MethodError::new(error_code::INVALID_PARAMS, error.to_string()),
        _ => org_error(error),
    }
}

fn route_result(state: &OrgState, seq: u64, id: MessageId) -> ChannelRouteResult {
    let woke = post(state, id)
        .into_iter()
        .flat_map(|p| p.woke())
        .filter_map(|to| {
            Some(WokeRow {
                to: to.clone(),
                message_id: wake_for(state, id, to)?,
            })
        })
        .collect();
    ChannelRouteResult { seq, woke }
}
