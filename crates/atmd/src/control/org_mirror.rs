//! Read-only legacy-input projection. Snapshot/watch I/O never runs in a tmux turn.
//! A snapshot has no cursor, so its pre-read high-water mark is a conservative
//! lower bound. Never authorize from it while the writer's cursor is newer.
//! Watch subscribes before snapshot; refresh (not re-apply) on later commits.
//! Transient read failures retain the projection and retry with a capped backoff;
//! only failed organisation health marks the mirror unavailable.
#[cfg(test)]
#[path = "org_mirror_tests.rs"]
mod tests;

use crate::org::OrgHandle;
use atm_org::OrgState;
use std::sync::Arc;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone)]
enum Projection {
    Loading,
    Ready { through: u64, state: Arc<OrgState> },
    Unavailable,
}
pub(super) enum View {
    Loading,
    Ready(Arc<OrgState>),
    Unavailable,
}
pub(super) struct Mirror {
    org: OrgHandle,
    projection: watch::Receiver<Projection>,
    worker: JoinHandle<()>,
    open: bool,
}
impl Mirror {
    pub(super) fn start(org: OrgHandle) -> Self {
        Self::start_with_snapshot(org, |org| async move { org.snapshot().await })
    }
    fn start_with_snapshot<F, Fut>(org: OrgHandle, mut snapshot: F) -> Self
    where
        F: FnMut(OrgHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<OrgState, crate::org::OrgServiceError>> + Send,
    {
        let (updates, projection) = watch::channel(Projection::Loading);
        let source = org.clone();
        let worker = tokio::spawn(async move {
            let mut backoff = std::time::Duration::from_millis(100);
            while refresh_with_snapshot(&source, &updates, &mut snapshot)
                .await
                .is_err()
            {
                if source.status().is_err() {
                    updates.send_replace(Projection::Unavailable);
                    break;
                }
                // Queue pressure, snapshot timeouts and replay read failures do
                // not change org health. Retain the last conservative cursor.
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(1));
            }
        });
        Self {
            org,
            projection,
            worker,
            open: true,
        }
    }
    pub(super) fn view(&self) -> View {
        let Ok(current) = self.org.status() else {
            return View::Unavailable;
        };
        match &*self.projection.borrow() {
            Projection::Ready { through, state } if *through >= current => {
                View::Ready(state.clone())
            }
            Projection::Unavailable => View::Unavailable,
            _ => View::Loading,
        }
    }
    pub(super) async fn changed(&mut self) {
        if !self.open {
            std::future::pending::<()>().await;
        }
        if self.projection.changed().await.is_err() {
            self.open = false;
        }
    }
    pub(super) async fn stop(self) {
        self.worker.abort();
        let _ = self.worker.await;
    }
}
#[cfg(test)]
async fn refresh(
    org: OrgHandle,
    updates: &watch::Sender<Projection>,
) -> Result<(), crate::org::OrgServiceError> {
    refresh_with_snapshot(&org, updates, &mut |org: OrgHandle| async move {
        org.snapshot().await
    })
    .await
}
async fn refresh_with_snapshot<F, Fut>(
    org: &OrgHandle,
    updates: &watch::Sender<Projection>,
    snapshot: &mut F,
) -> Result<(), crate::org::OrgServiceError>
where
    F: FnMut(OrgHandle) -> Fut,
    Fut: std::future::Future<Output = Result<OrgState, crate::org::OrgServiceError>>,
{
    let mut events = org.watch(Vec::new(), org.status()?)?;
    loop {
        let through = org.status()?;
        let state = snapshot(org.clone()).await?;
        if updates
            .send(Projection::Ready {
                through,
                state: Arc::new(state),
            })
            .is_err()
        {
            return Ok(());
        }
        // Concurrent commits can put the snapshot ahead of its conservative
        // cursor. Refresh until the bound catches up; never guess its exact seq.
        if org.status()? != through {
            continue;
        }
        loop {
            match events.next().await? {
                Some(event) if event.seq <= through => continue,
                Some(_) => break,
                None => {
                    return Err(crate::org::OrgServiceError::Unavailable(
                        "organisation watch closed".into(),
                    ))
                }
            }
        }
    }
}
