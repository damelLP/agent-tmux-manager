//! A single budget pass; T3's scheduler owns every wake and calls this serially.
//! A normal committed msg.sent is the latch. After any uncertain enqueue, the
//! next pass snapshots through the actor's FIFO before deciding to send again.

use super::{OrgHandle, OrgServiceError};
use atm_org::{budget::budget_step, OrgCommand};
use chrono::{DateTime, Utc};

pub(super) async fn reconcile_budgets(
    org: &OrgHandle,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, OrgServiceError> {
    let state = org.snapshot().await?;
    // This base has no running task ledger yet: usage is genuinely unavailable.
    // T4a supplies the sole task_cost accessor; deadlines need no usage metric.
    for note in budget_step(&state, now, |_| None).notes {
        org.execute(OrgCommand::SendMessage(note), now).await?;
    }
    let state = org.snapshot().await?;
    Ok(budget_step(&state, now, |_| None).wake_at)
}
