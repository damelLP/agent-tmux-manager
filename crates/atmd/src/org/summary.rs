//! Last-known session facts; not a complete lifecycle history.
//! Cost is recorded at $0.05 from the last summary; clean shutdown flushes the
//! exact latest facts without ending live sessions. A crash can lose the smaller
//! cost delta (and any not-yet-committed update). First prompt/start survive resume.
//! Initial zero cannot be distinguished from a genuine zero correction on resume:
//! keep recovered cost until the first nonzero live cost, then accept decreases.
//! Broadcast lag resynchronizes from the registry, never disables organisation
//! methods. End times are observation times, not harness occurrence timestamps.

use super::OrgHandle;
use crate::registry::{RegistryHandle, RemovalReason, SessionEvent};
use atm_core::{builtin_harnesses, Harness, Money, SessionId, SessionView};
use atm_org::{OrgCommand, OrgState, SessionSummary};
use chrono::{DateTime, Utc};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::{
    sync::broadcast,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

/// Subscribe before discovery. Finish while the registry and org actor still
/// live; dropping/aborting the daemon must not invent session end events.
#[must_use]
pub struct SummaryObserver {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl SummaryObserver {
    pub fn spawn(registry: RegistryHandle, org: OrgHandle) -> Self {
        let events = registry.subscribe();
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(registry, org, events, stop.clone()));
        Self { stop, task }
    }

    pub async fn finish(mut self) {
        self.stop.cancel();
        // Connection drain takes at most 2 s; leave 1 s of margin inside
        // `atmd stop`'s 5 s wait for the remaining shutdown work.
        match tokio::time::timeout(Duration::from_secs(2), &mut self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "Session summary observer failed"),
            Err(_) => {
                self.task.abort();
                tracing::warn!("Session summary flush timed out; persistence may be incomplete");
            }
        }
    }
}

fn from_view(view: &SessionView) -> Option<SessionSummary> {
    if view.id.is_pending() {
        return None;
    }
    Some(SessionSummary {
        session_id: view.id.clone(),
        pane: view.tmux_pane.clone(),
        agent_id: None,
        harness: builtin_harnesses()
            .find(|h| h.harness.short_tag() == view.harness)
            .map(|h| h.harness)
            .unwrap_or(Harness::Unknown),
        model: (!view.model.is_empty() && view.model != atm_core::Model::Unknown.display_name())
            .then(|| view.model.clone()),
        cost: view
            .cost_usd
            .is_finite()
            .then(|| Money::from_usd(view.cost_usd)),
        first_prompt: view.first_prompt.clone().filter(|s| !s.is_empty()),
        started: DateTime::parse_from_rfc3339(&view.started_at)
            .ok()?
            .with_timezone(&Utc),
        ended: None,
    })
}

struct Entry {
    latest: SessionSummary,
    saved: Option<SessionSummary>,
    live_cost_seen: bool,
}

impl Entry {
    fn new(live: SessionSummary, recovered: Option<&SessionSummary>) -> Self {
        let mut entry = Self {
            latest: recovered.cloned().unwrap_or_else(|| live.clone()),
            saved: recovered.cloned(),
            live_cost_seen: false,
        };
        entry.update(live);
        entry
    }

    fn update(&mut self, mut live: SessionSummary) {
        self.live_cost_seen |= live.cost.is_some_and(|cost| cost.as_microdollars() != 0);
        if !self.live_cost_seen {
            live.cost = self.latest.cost.or(live.cost);
        }
        live.started = self.latest.started;
        live.first_prompt = self.latest.first_prompt.clone().or(live.first_prompt);
        live.model = live.model.or_else(|| self.latest.model.clone());
        live.pane = live.pane.or_else(|| self.latest.pane.clone());
        live.agent_id = self.latest.agent_id.clone();
        self.latest = live;
    }

    fn due(&self, flush: bool) -> bool {
        let Some(saved) = &self.saved else {
            return true;
        };
        if flush {
            return self.latest != *saved;
        }
        let mut without_cost = self.latest.clone();
        without_cost.cost = saved.cost;
        without_cost != *saved
            || match (self.latest.cost, saved.cost) {
                (Some(a), Some(b)) => a.as_microdollars().abs_diff(b.as_microdollars()) >= 50_000,
                (a, b) => a != b,
            }
    }
}

// Bound transient unpersisted rows, including completed sessions during a slow
// write. Overflow loses last-known facts with an explicit warning, never org
// availability. The durable OrgState itself intentionally retains all summaries.
const MAX_PENDING: usize = 4096;
type Entries = HashMap<SessionId, Entry>;

struct Recovery {
    startup: OrgState,
    saved: HashMap<SessionId, SessionSummary>,
}

fn update(entries: &mut Entries, recovered: &Recovery, view: &SessionView) {
    let Some(summary) = from_view(view) else {
        return;
    };
    if let Some(entry) = entries.get_mut(&view.id) {
        entry.update(summary);
        return;
    }
    if entries.len() >= MAX_PENDING {
        tracing::warn!("Session summary cache full; dropping new snapshot");
        return;
    }
    entries.insert(
        view.id.clone(),
        Entry::new(
            summary,
            recovered
                .saved
                .get(&view.id)
                .or_else(|| recovered.startup.session_summary(&view.id)),
        ),
    );
}

async fn resync(
    registry: &RegistryHandle,
    entries: &mut Entries,
    recovered: &Recovery,
    now: DateTime<Utc>,
) {
    let views = registry.get_all_sessions().await;
    if !registry.is_connected() {
        // The handle returns an empty list on actor loss, not a valid snapshot.
        return;
    }
    let present: HashSet<_> = views.iter().map(|v| v.id.clone()).collect();
    for view in views {
        update(entries, recovered, &view);
    }
    for (id, entry) in entries {
        if !present.contains(id) && entry.latest.ended.is_none() {
            entry.latest.ended = Some(now);
        }
    }
}

async fn observe(
    event: SessionEvent,
    registry: &RegistryHandle,
    entries: &mut Entries,
    recovered: &Recovery,
    now: DateTime<Utc>,
) {
    match event {
        SessionEvent::Updated { session } => update(entries, recovered, &session),
        SessionEvent::Registered { session_id, .. } => {
            // Registered has no snapshot. The next Updated normally supplies
            // it; lookup also handles generic registration that has no update.
            if let Some(view) = registry.get_session(session_id).await {
                update(entries, recovered, &view);
            }
        }
        SessionEvent::Removed { session_id, reason } => {
            if reason != RemovalReason::Upgraded {
                if let Some(entry) = entries.get_mut(&session_id) {
                    entry.latest.ended = Some(now);
                }
            }
        }
    }
}

async fn run(
    registry: RegistryHandle,
    org: OrgHandle,
    mut events: broadcast::Receiver<SessionEvent>,
    stop: CancellationToken,
) {
    // Read-only recovery defaults, overlaid with summaries saved in this run.
    // The org actor remains the sole authority for decisions and durable state.
    let startup = match org.snapshot().await {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(%error, "Session summaries unavailable");
            return;
        }
    };
    let mut recovered = Recovery {
        startup,
        saved: HashMap::new(),
    };
    let mut entries = Entries::new();
    let mut pending = JoinSet::new();
    let mut flushing = false;
    let mut retry_at = None;
    loop {
        if pending.is_empty() && retry_at.is_none() {
            let next = entries
                .values()
                .find(|entry| entry.due(flushing))
                .map(|entry| entry.latest.clone());
            if let Some(summary) = next {
                let org = org.clone();
                pending.spawn(async move {
                    let result = org
                        .execute(OrgCommand::RecordSummary(summary.clone()), Utc::now())
                        .await;
                    (summary, result)
                });
            } else if flushing {
                break;
            }
        }
        tokio::select! {
            biased;
            _ = stop.cancelled(), if !flushing => {
                flushing = true;
                retry_at = None;
                // Drain boundary events before the final snapshot: removed rows
                // are no longer in the registry and must retain their last facts.
                loop {
                    match events.try_recv() {
                        Ok(event) => observe(event, &registry, &mut entries, &recovered, Utc::now()).await,
                        Err(broadcast::error::TryRecvError::Lagged(skipped)) => tracing::warn!(skipped, "Session summaries lagged during shutdown; resynchronizing"),
                        Err(_) => break,
                    }
                }
                resync(&registry, &mut entries, &recovered, Utc::now()).await;
            },
            result = pending.join_next(), if !pending.is_empty() => {
                match result {
                    Some(Ok((summary, Ok(_)))) => {
                        recovered.saved.insert(summary.session_id.clone(), summary.clone());
                        if let Some(entry) = entries.get_mut(&summary.session_id) {
                            entry.saved = Some(summary.clone());
                            if summary.ended.is_some() && entry.latest == summary { entries.remove(&summary.session_id); }
                        }
                    },
                    Some(Ok((_, Err(error)))) => {
                        if org.status().is_err() {
                            tracing::warn!(%error, "Session summary persistence stopped");
                            break;
                        }
                        // Refusal/overload/timeout is not service death. Keep the
                        // dirty entry and consume updates while backing off, so
                        // an invalid snapshot can be corrected without a hot loop.
                        tracing::warn!(%error, "Session summary deferred; will retry");
                        retry_at = Some(tokio::time::Instant::now() + Duration::from_millis(100));
                    },
                    Some(Err(error)) => { tracing::warn!(%error, "Session summary task failed"); break; },
                    None => {},
                }
            },
            _ = async {
                if let Some(at) = retry_at { tokio::time::sleep_until(at).await; }
            }, if retry_at.is_some() => { retry_at = None; },
            event = events.recv(), if !flushing => match event {
                Ok(event) => observe(event, &registry, &mut entries, &recovered, Utc::now()).await,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "Session summaries lagged; resynchronizing");
                    resync(&registry, &mut entries, &recovered, Utc::now()).await;
                },
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
