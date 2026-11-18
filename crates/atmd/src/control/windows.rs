//! Caller-independent, registry-bounded window facts; no role reads or mutations.

use super::service::ControlHandle;
use crate::{
    registry::RegistryHandle,
    server::methods::{parse_params, MethodError, MethodRouter},
};
use atm_core::SessionView;
use atm_tmux::PaneWindow;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, time::Instant};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WindowsSnapshot {
    generated_at: DateTime<Utc>,
    state: &'static str,
    reason: Option<String>,
    windows: Vec<Window>,
}
#[derive(Debug, Clone, Serialize)]
struct Window {
    id: String,
    name: String,
    links: Vec<Link>,
    panes: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct Link {
    session: String,
    index: u32,
}
fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

pub(super) fn unavailable(now: DateTime<Utc>, reason: &str) -> WindowsSnapshot {
    WindowsSnapshot {
        generated_at: now,
        state: "unavailable",
        reason: Some(clean(reason)),
        windows: vec![],
    }
}

/// Only registered panes are exposed; each window/link/pane contributes once.
pub(super) fn project(
    rows: &[SessionView],
    panes: &[PaneWindow],
    now: DateTime<Utc>,
) -> WindowsSnapshot {
    let live: BTreeSet<_> = rows
        .iter()
        .filter_map(|row| row.tmux_pane.as_deref())
        .collect();
    let mut grouped = BTreeMap::<&str, Window>::new();
    for pane in panes
        .iter()
        .filter(|pane| live.contains(pane.pane_id.as_str()))
    {
        let window = grouped.entry(&pane.window_id).or_insert_with(|| Window {
            id: pane.window_id.clone(),
            name: clean(&pane.window_name),
            links: vec![],
            panes: vec![],
        });
        window.links.push(Link {
            session: pane.session_name.clone(),
            index: pane.window_index,
        });
        window.panes.push(pane.pane_id.clone());
    }
    let mut windows: Vec<_> = grouped.into_values().collect();
    for window in &mut windows {
        window.links.sort();
        window.links.dedup();
        window.panes.sort();
        window.panes.dedup();
    }
    windows.sort_by(|a, b| {
        a.links.first().cmp(&b.links.first()).then_with(|| {
            let number = |id: &str| id.strip_prefix('@').and_then(|n| n.parse::<u64>().ok());
            number(&a.id).cmp(&number(&b.id))
        })
    });
    WindowsSnapshot {
        generated_at: now,
        state: "ok",
        reason: None,
        windows,
    }
}

pub(super) const BUDGET: Duration = Duration::from_secs(5);
type Observation = watch::Receiver<Option<WindowsSnapshot>>;
#[derive(Debug)]
struct Shared {
    deadline: Instant,
    receive: Observation,
}
/// The slot holds a receiver only: actor/queue loss must close and wake waiters.
#[derive(Debug, Clone, Default)]
pub(super) struct Flights(Arc<Mutex<Option<Shared>>>);
pub(super) struct Subscription {
    pub deadline: Instant,
    pub receive: Observation,
    pub start: Option<Flight>,
}
pub(super) struct Flight(watch::Sender<Option<WindowsSnapshot>>);
impl Flights {
    pub fn subscribe(&self, now: Instant) -> Option<Subscription> {
        let mut slot = self.0.lock().ok()?;
        if let Some(shared) = slot.as_ref().filter(|shared| {
            shared.receive.borrow().is_none() && shared.receive.has_changed().is_ok()
        }) {
            return Some(Subscription {
                deadline: shared.deadline,
                receive: shared.receive.clone(),
                start: None,
            });
        }
        let deadline = now + BUDGET;
        let (send, receive) = watch::channel(None);
        *slot = Some(Shared {
            deadline,
            receive: receive.clone(),
        });
        Some(Subscription {
            deadline,
            receive,
            start: Some(Flight(send)),
        })
    }
}
impl Flight {
    pub fn finish(self, value: WindowsSnapshot) {
        let _ = self.0.send(Some(value));
    }
}
pub(super) async fn receive(mut receive: Observation) -> WindowsSnapshot {
    loop {
        if let Some(value) = receive.borrow().clone() {
            return value;
        }
        if receive.changed().await.is_err() {
            return unavailable(Utc::now(), "window reader closed");
        }
    }
}
/// The actor owns this bounded future, so there is nothing detached to clean up.
pub(super) async fn read(
    client: &dyn atm_tmux::TmuxClient,
    registry: &RegistryHandle,
    deadline: Instant,
) -> WindowsSnapshot {
    if Instant::now() >= deadline {
        return unavailable(Utc::now(), "window read timed out in queue");
    }
    let rows = async {
        if !registry.is_connected() {
            return Err("session registry unavailable");
        }
        let rows = registry.get_all_sessions().await;
        if !registry.is_connected() {
            return Err("session registry unavailable");
        }
        Ok(rows)
    };
    let panes = async {
        client
            .list_pane_windows()
            .await
            .map_err(|error| match error {
                atm_tmux::TmuxError::NotFound => "tmux is not installed",
                atm_tmux::TmuxError::Timeout { .. } => "tmux window read timed out",
                atm_tmux::TmuxError::ParseError(_) => "malformed tmux window output",
                _ => "tmux window read failed",
            })
    };
    match tokio::time::timeout_at(deadline, async { tokio::try_join!(rows, panes) }).await {
        Ok(Ok((rows, panes))) => project(&rows, &panes, Utc::now()),
        Ok(Err(reason)) => unavailable(Utc::now(), reason),
        Err(_) => unavailable(Utc::now(), "window read timed out"),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {}
pub(crate) fn register(
    router: &mut MethodRouter,
    control: Option<ControlHandle>,
) -> Result<(), MethodError> {
    router.register(
        "session.windows",
        Duration::from_secs(6),
        move |_, params| {
            let control = control.clone();
            async move {
                parse_params::<Params>(params)?;
                let value = match control {
                    Some(control) => control.windows().await,
                    None => unavailable(Utc::now(), "control service unavailable"),
                };
                serde_json::to_value(value).map_err(|error| {
                    MethodError::new(atm_protocol::error_code::INTERNAL, error.to_string())
                })
            }
        },
    )
}

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
