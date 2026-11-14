//! EVENTS-only cursor reads. Cursor zero means all retained history; topics
//! match kind prefixes (an empty list means all). Timestamps on watch frames
//! are RFC3339 UTC with `Z`, preserving subsecond precision.

use std::{collections::VecDeque, sync::Arc};

use serde::Deserialize;
use tokio::sync::broadcast;

use super::{storage::Reader, OrgHandle, OrgServiceError};
use crate::eventlog::{Envelope, EventLog};

pub(super) const TAIL_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug)]
pub(super) struct ReadAccess {
    pub log: EventLog,
    pub reader: Reader,
}

/// Count and byte bounded pages. Continue after the last returned sequence.
/// An empty result means no matches at the captured committed high-water mark,
/// not that no future events can match. Raw logs are not exposed here.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TailParams {
    pub after_seq: u64,
    pub limit: usize,
    pub topics: Vec<String>,
}

impl Default for TailParams {
    fn default() -> Self {
        Self {
            after_seq: 0,
            limit: 100,
            topics: Vec::new(),
        }
    }
}

impl OrgHandle {
    fn reads(&self) -> Result<&ReadAccess, OrgServiceError> {
        self.status()?;
        self.reads
            .as_ref()
            .ok_or_else(|| OrgServiceError::Unavailable("log reader not configured".into()))
    }

    pub async fn tail(&self, params: TailParams) -> Result<Vec<Envelope>, OrgServiceError> {
        if !(1..=1000).contains(&params.limit) {
            return Err(OrgServiceError::InvalidParams(
                "limit must be between 1 and 1000".into(),
            ));
        }
        let reads = self.reads()?;
        let fence = self.status()?;
        let mut scanned = params.after_seq;
        let mut result = Vec::new();
        let mut bytes = 2; // JSON array brackets; one extra comma per record is conservative.
        while scanned < fence {
            let page = reads.reader.page(scanned, fence, &params.topics).await?;
            self.status()?;
            scanned = page.scanned;
            for record in page.records {
                let size = serde_json::to_vec(&record)
                    .map_err(|e| OrgServiceError::Unavailable(e.to_string()))?
                    .len()
                    + 1;
                if bytes + size > TAIL_BYTES {
                    return if result.is_empty() {
                        Err(OrgServiceError::Oversized(record.seq))
                    } else {
                        Ok(result)
                    };
                }
                bytes += size;
                result.push(record);
                if result.len() == params.limit {
                    return Ok(result);
                }
            }
        }
        Ok(result)
    }

    /// Subscribe BEFORE capturing the replay fence. Replay and live records
    /// overlap deliberately; EventWatch discards all seq <= its scanned cursor.
    pub fn watch(&self, topics: Vec<String>, cursor: u64) -> Result<EventWatch, OrgServiceError> {
        let live = self.reads()?.log.subscribe();
        let fence = self.status()?;
        Ok(EventWatch {
            org: self.clone(),
            live,
            topics,
            cursor,
            fence,
            replay: VecDeque::new(),
            pending: None,
        })
    }
}

/// A watch holds at most one replay page and one pending live record. Dropping
/// it cancels future delivery, not the shared filesystem worker. `next` may be
/// cancelled safely: pending live data survives a cancelled ACK wait.
pub struct EventWatch {
    org: OrgHandle,
    live: broadcast::Receiver<Arc<Envelope>>,
    topics: Vec<String>,
    cursor: u64,
    fence: u64,
    replay: VecDeque<Arc<Envelope>>,
    pending: Option<Arc<Envelope>>,
}

impl EventWatch {
    pub async fn next(&mut self) -> Result<Option<Arc<Envelope>>, OrgServiceError> {
        loop {
            let committed = self.org.status()?;
            if let Some(record) = self.replay.pop_front() {
                return Ok(Some(record));
            }
            if self.cursor < self.fence {
                let page = self
                    .org
                    .reads()?
                    .reader
                    .page(self.cursor, self.fence, &self.topics)
                    .await?;
                self.org.status()?;
                self.cursor = page.scanned;
                self.replay.extend(page.records.into_iter().map(Arc::new));
                continue;
            }
            if let Some(record) = &self.pending {
                if record.seq <= self.cursor {
                    self.pending = None;
                    continue;
                }
                if record.seq <= committed {
                    self.cursor = record.seq;
                    if matches(&self.topics, &record.kind) {
                        return Ok(self.pending.take());
                    }
                    self.pending = None;
                    continue;
                }
                self.org
                    .health
                    .changed()
                    .await
                    .map_err(|_| self.org.closed_error())?;
                continue;
            }
            tokio::select! {
                changed = self.org.health.changed() => {
                    changed.map_err(|_| self.org.closed_error())?;
                }
                record = self.live.recv() => match record {
                    Ok(record) => self.pending = Some(record),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Keep this subscription: the new replay may itself
                        // overlap live records or lag again during catch-up.
                        self.fence = self.org.status()?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(None),
                }
            }
        }
    }
}

pub(super) fn matches(topics: &[String], kind: &str) -> bool {
    topics.is_empty() || topics.iter().any(|prefix| kind.starts_with(prefix))
}

pub(crate) fn message(record: &Envelope) -> atm_protocol::DaemonMessage {
    atm_protocol::DaemonMessage::event(
        record
            .kind
            .split('.')
            .next()
            .unwrap_or(&record.kind)
            .to_owned(),
        record.seq,
        record
            .ts
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        record.kind.clone(),
        record.body.clone(),
    )
}
