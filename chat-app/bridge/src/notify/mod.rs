mod push;
#[cfg(test)]
mod push_tests;
#[cfg(test)]
mod state_tests;
mod store;
mod watch;
#[cfg(test)]
mod watch_tests;

use anyhow::Result;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::json;
use std::{future::Future, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, Notify, Semaphore},
    time::{sleep, timeout, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::{files::detached, http::json_response, relay::IO};
use push::{Endpoint, Key, TTL};
use watch::{Fact, Notice, Preview};

pub(crate) const SUBSCRIPTION: &str = "/notify/subscription";

#[derive(Default)]
struct Slot {
    epoch: u64,
    endpoint: Option<Endpoint>,
    waiting: Option<Notice>,
    offered: Option<(Notice, Instant)>,
}

enum Change {
    Set(Endpoint, bool),
    Gone(u64),
}

pub(crate) struct Notifications {
    dir: PathBuf,
    socket: PathBuf,
    key: Key,
    public: String,
    slot: Mutex<Slot>,
    writer: Arc<Semaphore>,
    ready: Notify,
}

impl Notifications {
    pub(crate) async fn start(
        dir: PathBuf,
        excluded: Vec<PathBuf>,
        socket: PathBuf,
        stop: CancellationToken,
    ) -> Result<Arc<Self>> {
        let n = Self::load(dir, excluded, socket).await?;
        let worker = n.clone();
        tokio::spawn(async move {
            let (key, cancel) = (&worker.key, &stop);
            tokio::join!(
                watch::follow(&worker.socket, &stop, |f| worker.observe(f)),
                worker.deliver(&stop, |endpoint| async move {
                    push::post(key, &endpoint, cancel).await
                })
            );
        });
        Ok(n)
    }

    async fn load(dir: PathBuf, excluded: Vec<PathBuf>, socket: PathBuf) -> Result<Arc<Self>> {
        let path = dir.clone();
        let (key, endpoint) =
            timeout(IO, detached(move || store::open(&path, excluded))?).await???;
        let public = push::public_key(&key)?;
        Ok(Arc::new(Self {
            dir,
            socket,
            key,
            public,
            slot: Mutex::new(Slot {
                endpoint,
                ..Slot::default()
            }),
            writer: Arc::new(Semaphore::new(1)),
            ready: Notify::new(),
        }))
    }

    async fn observe(&self, fact: Fact) {
        let mut slot = self.slot.lock().await;
        if slot.endpoint.is_none() {
            return;
        }
        if slot.waiting.as_ref().is_some_and(|n| n.cleared_by(&fact)) {
            slot.waiting = None;
        }
        if slot
            .offered
            .as_ref()
            .is_some_and(|(n, _)| n.cleared_by(&fact))
        {
            slot.offered = None;
        }
        if let Some(notice) = Notice::select(&fact) {
            slot.waiting = Some(notice);
            self.ready.notify_one();
        }
    }

    async fn deliver<F: Future<Output = Result<u16>>>(
        self: &Arc<Self>,
        stop: &CancellationToken,
        mut post: impl FnMut(Endpoint) -> F,
    ) {
        while stop
            .run_until_cancelled(self.ready.notified())
            .await
            .is_some()
        {
            if stop
                .run_until_cancelled(sleep(Duration::from_secs(1)))
                .await
                .is_none()
            {
                break;
            }
            let (Some(endpoint), Some(notice), epoch) = ({
                let slot = self.slot.lock().await;
                (slot.endpoint.clone(), slot.waiting.clone(), slot.epoch)
            }) else {
                continue;
            };
            let Some(send) = preview_for_push(stop, || notice.preview(&self.socket)).await else {
                break;
            };
            {
                let mut slot = self.slot.lock().await;
                if slot.epoch != epoch || slot.waiting.as_ref() != Some(&notice) {
                    continue;
                }
                slot.waiting = None;
                if !send {
                    continue;
                }
                // Publish before POST: the wake can reach the phone before curl reads the response.
                slot.offered = Some((notice, Instant::now() + Duration::from_secs(TTL)));
            }
            match post(endpoint).await {
                Ok(404 | 410) => {
                    let _ = self.clone().write_slot(Change::Gone(epoch)).await;
                }
                Ok(200..=299) => {}
                _ if !stop.is_cancelled() => eprintln!("Chat notification push unavailable"),
                _ => {}
            }
        }
    }

    async fn write_slot(self: Arc<Self>, change: Change) -> Result<()> {
        // Even if HTTP disconnects or the disk stalls, only this one transaction can write.
        let permit = self.writer.clone().try_acquire_owned()?;
        let job = tokio::spawn(async move {
            let _permit = permit;
            let slot = self.slot.lock().await;
            let next = match change {
                Change::Set(ep, true) if slot.endpoint.as_ref() != Some(&ep) => Some(ep),
                Change::Set(ep, false) if slot.endpoint.as_ref() == Some(&ep) => None,
                Change::Gone(epoch) if slot.epoch == epoch => None,
                _ => return Ok(()),
            };
            let epoch = slot.epoch.wrapping_add(1);
            drop(slot);
            let (dir, disk) = (self.dir.clone(), next.clone());
            detached(move || store::set(&dir, disk))?.await??;
            *self.slot.lock().await = Slot {
                epoch,
                endpoint: next,
                ..Slot::default()
            };
            Ok(())
        });
        timeout(IO, job).await??
    }
}

async fn preview_for_push<F: Future<Output = Result<Option<Preview>>>>(
    stop: &CancellationToken,
    mut read: impl FnMut() -> F,
) -> Option<bool> {
    stop.run_until_cancelled(async {
        for _ in 0..2 {
            if let Ok(Ok(preview)) = timeout(IO, read()).await {
                return preview.is_some();
            }
        }
        eprintln!("Chat notification preview unavailable after retry; sending empty push");
        true
    })
    .await
}

pub(crate) fn routes<S: Clone + Send + Sync + 'static>(n: Arc<Notifications>) -> Router<S> {
    Router::new()
        .route(
            SUBSCRIPTION,
            get(settings)
                .post(change)
                .layer(DefaultBodyLimit::max(2048)),
        )
        .route("/notify/latest", get(latest))
        .with_state(n)
}

async fn settings(State(n): State<Arc<Notifications>>) -> Response {
    let slot = n.slot.lock().await;
    json_response(
        StatusCode::OK,
        json!({"key":n.public,"endpoint":slot.endpoint.as_ref().map(Endpoint::as_str)}).to_string(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    endpoint: String,
    on: bool,
}

async fn change(State(n): State<Arc<Notifications>>, body: Bytes) -> StatusCode {
    let parsed = (|| -> Result<Change> {
        let req: Subscription = serde_json::from_slice(&body)?;
        Ok(Change::Set(Endpoint::parse(&req.endpoint)?, req.on))
    })();
    let Ok(change) = parsed else {
        return StatusCode::BAD_REQUEST;
    };
    match n.write_slot(change).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn latest(State(n): State<Arc<Notifications>>) -> Response {
    let message = async {
        let offered = n.slot.lock().await.offered.clone()?;
        let (notice, until) = &offered;
        if Instant::now() >= *until {
            return None;
        }
        let preview = notice.preview(&n.socket).await.ok()??;
        let current = n.slot.lock().await;
        (current.offered.as_ref() == Some(&offered) && Instant::now() < *until).then_some(preview)
    };
    if let Some(body) = timeout(Duration::from_secs(3), message)
        .await
        .ok()
        .flatten()
        .and_then(|p| serde_json::to_string(&p).ok())
    {
        return json_response(StatusCode::OK, body);
    }
    (
        StatusCode::NO_CONTENT,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
    )
        .into_response()
}
