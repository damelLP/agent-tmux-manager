//! Sequence-based absences and the latest immutable, replayable return report.
//! Recorded dependency policy is shared with caps/overruling, never recomputed
//! by clients. A report is frozen by one committed owner.digest_created fact.
use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwayWindow {
    pub away_seq: u64,
    pub back_seq: u64,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDigestCreated {
    pub by: Address,
    pub back_seq: Option<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestSection {
    Taken,
    Relayed,
    Earlier,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestCounts {
    pub during_absence: usize,
    #[serde(default)]
    pub relayed: usize,
    pub earlier_provisional: usize,
    pub waiting_decisions: usize,
    pub waiting_tasks: usize,
    pub waiting_total: usize,
    pub taken_for_owner: usize,
    pub overruled: usize,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactCounts {
    pub started: usize,
    pub accepted: usize,
    pub not_started: usize,
    pub cancelled: usize,
    pub unknown: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DigestRow {
    Decision {
        section: DigestSection,
        decision: Box<Decision>,
        impact: ImpactCounts,
        answer_caller_pane: Option<String>,
    },
    Task {
        decision_id: DecisionId,
        task_id: TaskId,
        title: String,
        state: TaskState,
        started: bool,
        direct: bool,
    },
    Waiting {
        decision: Box<Decision>,
    },
    WaitingTask {
        task_id: TaskId,
        title: String,
        state: TaskState,
        reason: Option<String>,
        held_on: Option<DecisionId>,
    },
    OverruleNumber {
        decision_id: DecisionId,
        completed: Option<usize>,
        required: Option<usize>,
        cancelled: Option<usize>,
        caller_pane: Option<String>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnDigest {
    pub snapshot_seq: u64,
    pub absence: Option<AwayWindow>,
    pub relay: Option<AgentId>,
    pub counts: DigestCounts,
    pub rows: Vec<DigestRow>,
    pub provisional_count: usize,
    pub pending_count: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestPage {
    pub snapshot_seq: u64,
    pub absence: Option<AwayWindow>,
    #[serde(default)]
    pub relay: Option<AgentId>,
    pub counts: DigestCounts,
    pub total_rows: usize,
    pub rows: Vec<DigestRow>,
    pub next_after: Option<u64>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReturnHistory {
    active: Option<(u64, DateTime<Utc>)>,
    windows: BTreeMap<u64, AwayWindow>,
    last: Option<u64>,
    takes: BTreeMap<DecisionId, (u64, Option<u64>)>,
    overrules: BTreeMap<DecisionId, u64>,
    callers: BTreeMap<DecisionId, Option<String>>,
    latest: Option<ReturnDigest>,
}
fn invalid(reason: impl Into<String>) -> Refusal {
    Refusal::InvalidValue {
        field: "return digest",
        reason: reason.into(),
    }
}
pub(crate) fn check_digest(state: &OrgState, back_seq: Option<u64>) -> Result<(), Refusal> {
    if state.owner().away_since.is_some() {
        return Err(invalid("back was recorded, but the owner became away before the digest was prepared; run atm back again"));
    }
    let history = &state.return_history;
    if back_seq.is_some_and(|seq| !history.windows.contains_key(&seq))
        || (back_seq.is_none() && history.last.is_some())
    {
        return Err(invalid("the referenced closed absence was not recorded"));
    }
    Ok(())
}
impl OrgState {
    #[must_use]
    pub fn return_digest(&self) -> Option<&ReturnDigest> {
        self.return_history.latest.as_ref()
    }
    #[must_use]
    pub fn away_window(&self, back_seq: u64) -> Option<&AwayWindow> {
        self.return_history.windows.get(&back_seq)
    }
    #[must_use]
    pub fn last_away_window(&self) -> Option<&AwayWindow> {
        self.return_history
            .last
            .and_then(|seq| self.return_history.windows.get(&seq))
    }
    pub(crate) fn track_return_event(&mut self, seq: u64, at: DateTime<Utc>, event: &OrgEvent) {
        let history = &mut self.return_history;
        match event {
            OrgEvent::OwnerAway(_) => history.active = Some((seq, at)),
            OrgEvent::OwnerBack(_) => {
                if let Some((away_seq, since)) = history.active.take() {
                    history.windows.insert(
                        seq,
                        AwayWindow {
                            away_seq,
                            back_seq: seq,
                            since,
                            until: at,
                        },
                    );
                    history.last = Some(seq);
                }
            }
            OrgEvent::DecisionTakenProvisionally(choice) => {
                history
                    .takes
                    .entry(choice.decision_id)
                    .or_insert((seq, history.active.map(|(seq, _)| seq)));
            }
            OrgEvent::DecisionResolved(answer) | OrgEvent::DecisionOverruled(answer) => {
                let id = answer.resolution.decision_id;
                history.callers.insert(id, answer.caller_pane.clone());
                if matches!(event, OrgEvent::DecisionOverruled(_))
                    && answer.resolution.by == Address::Owner
                    && history.takes.contains_key(&id)
                {
                    history.overrules.entry(id).or_insert(seq);
                }
            }
            _ => {}
        }
    }
    pub(crate) fn apply_digest(
        &mut self,
        seq: u64,
        back_seq: Option<u64>,
    ) -> Result<(), ApplyError> {
        let digest = build_return_digest(self, back_seq, seq)
            .map_err(|error| ApplyError::InvalidDigest(error.to_string()))?;
        self.return_history.latest = Some(digest);
        Ok(())
    }
}

pub fn build_return_digest(
    state: &OrgState,
    back_seq: Option<u64>,
    snapshot_seq: u64,
) -> Result<ReturnDigest, Refusal> {
    check_digest(state, back_seq)?;
    let history = &state.return_history;
    let absence = back_seq.and_then(|seq| history.windows.get(&seq)).cloned();
    let during = |id: DecisionId| {
        absence.as_ref().is_some_and(|window| {
            history.takes.get(&id).is_some_and(|(seq, away)| {
                *away == Some(window.away_seq) && window.away_seq < *seq && *seq < window.back_seq
            })
        })
    };
    let previous_snapshot = history.latest.as_ref().map(|digest| digest.snapshot_seq);
    let relayed = |decision: &Decision| {
        decision
            .provisional
            .as_ref()
            .is_some_and(|choice| choice.relayed_by.is_some())
            && history
                .takes
                .get(&decision.id)
                .is_some_and(|(seq, _)| previous_snapshot.is_none_or(|previous| *seq > previous))
    };
    let mut digest = ReturnDigest {
        snapshot_seq,
        absence: absence.clone(),
        relay: state.owner().relay.clone(),
        counts: DigestCounts {
            taken_for_owner: history.takes.len(),
            overruled: history.overrules.len(),
            ..DigestCounts::default()
        },
        rows: vec![],
        provisional_count: state
            .decisions()
            .filter(|d| d.status == DecisionStatus::TakenProvisionally)
            .count(),
        pending_count: state
            .decisions()
            .filter(|d| d.status == DecisionStatus::Pending)
            .count(),
    };
    for section in [
        DigestSection::Taken,
        DigestSection::Relayed,
        DigestSection::Earlier,
    ] {
        let mut decisions: Vec<_> = state
            .decisions()
            .filter(|d| match section {
                DigestSection::Taken => during(d.id),
                DigestSection::Relayed => !during(d.id) && relayed(d),
                DigestSection::Earlier => {
                    d.status == DecisionStatus::TakenProvisionally && !during(d.id) && !relayed(d)
                }
            })
            .map(|d| (d, decision_impact(state, d.id)))
            .collect();
        decisions.sort_by(|(a, ai), (b, bi)| {
            bi.started
                .len()
                .cmp(&ai.started.len())
                .then_with(|| bi.not_started.len().cmp(&ai.not_started.len()))
                .then_with(|| {
                    history
                        .takes
                        .get(&a.id)
                        .map(|v| v.0)
                        .cmp(&history.takes.get(&b.id).map(|v| v.0))
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        match section {
            DigestSection::Taken => digest.counts.during_absence = decisions.len(),
            DigestSection::Relayed => digest.counts.relayed = decisions.len(),
            DigestSection::Earlier => digest.counts.earlier_provisional = decisions.len(),
        }
        for (decision, impact) in decisions {
            let accepted = impact
                .started
                .iter()
                .filter(|id| {
                    state
                        .task(id)
                        .is_some_and(|t| t.state == TaskState::Accepted)
                })
                .count();
            digest.rows.push(DigestRow::Decision {
                section,
                decision: Box::new(decision.clone()),
                impact: ImpactCounts {
                    started: impact.started.len(),
                    accepted,
                    not_started: impact.not_started.len(),
                    cancelled: impact.cancelled.len(),
                    unknown: impact.unknown.len(),
                },
                answer_caller_pane: history.callers.get(&decision.id).cloned().flatten(),
            });
            for (ids, started) in [(&impact.started, true), (&impact.not_started, false)] {
                for id in ids {
                    if let Some(task) = state.task(id) {
                        digest.rows.push(DigestRow::Task {
                            decision_id: decision.id,
                            task_id: task.id.clone(),
                            title: task.title.clone(),
                            state: task.state,
                            started,
                            direct: task.rests_on == Some(decision.id),
                        });
                    }
                }
            }
        }
    }
    // Request sequence is creation order, independent of a backwards clock.
    for decision in state
        .decisions()
        .filter(|d| d.status == DecisionStatus::Pending)
    {
        digest.rows.push(DigestRow::Waiting {
            decision: Box::new(decision.clone()),
        });
        digest.counts.waiting_decisions += 1;
    }
    for task in state.tasks() {
        let held_on = match state.stacking_hold(task) {
            Err(AwayRefusal::Held { decision_id, .. }) => Some(decision_id),
            _ => None,
        };
        if held_on.is_some() || matches!(task.state, TaskState::Waiting | TaskState::Blocked) {
            digest.rows.push(DigestRow::WaitingTask {
                task_id: task.id.clone(),
                title: task.title.clone(),
                state: task.state,
                reason: task.reason.clone(),
                held_on,
            });
            digest.counts.waiting_tasks += 1;
        }
    }
    digest.counts.waiting_total = digest.counts.waiting_decisions + digest.counts.waiting_tasks;
    let mut overrules: Vec<_> = history.overrules.iter().collect();
    overrules.sort_by_key(|(id, seq)| (**seq, **id));
    for (id, _) in overrules {
        if let Some(progress) = state.decision_redo(*id) {
            let measured = progress.required.is_some();
            digest.rows.push(DigestRow::OverruleNumber {
                decision_id: *id,
                completed: measured.then_some(progress.completed.len()),
                required: progress.required.as_ref().map(Vec::len),
                cancelled: measured.then_some(progress.cancelled.len()),
                caller_pane: progress.caller_pane,
            });
        }
    }
    Ok(digest)
}

#[derive(Debug, Error)]
pub enum DigestPageError {
    #[error("limit must be between 1 and 1000")]
    InvalidLimit,
    #[error("unknown return digest cursor {0}")]
    InvalidCursor(u64),
    #[error("return digest row {row} exceeds the {byte_budget} byte budget")]
    OversizedRow { row: usize, byte_budget: usize },
    #[error("return digest metadata exceeds the {0} byte budget")]
    OversizedMetadata(usize),
    #[error("encoding return digest: {0}")]
    Encoding(#[from] serde_json::Error),
}
pub fn digest_page(
    digest: &ReturnDigest,
    after: Option<u64>,
    limit: u16,
    byte_budget: usize,
) -> Result<DigestPage, DigestPageError> {
    if !(1..=1000).contains(&limit) {
        return Err(DigestPageError::InvalidLimit);
    }
    let start = match after {
        None => 0,
        Some(after) => usize::try_from(after)
            .ok()
            .filter(|index| *index < digest.rows.len())
            .and_then(|index| index.checked_add(1))
            .ok_or(DigestPageError::InvalidCursor(after))?,
    };
    let mut page = DigestPage {
        snapshot_seq: digest.snapshot_seq,
        absence: digest.absence.clone(),
        relay: digest.relay.clone(),
        counts: digest.counts.clone(),
        total_rows: digest.rows.len(),
        rows: vec![],
        next_after: None,
    };
    if serde_json::to_vec(&page)?.len() > byte_budget {
        return Err(DigestPageError::OversizedMetadata(byte_budget));
    }
    for (index, row) in digest.rows.iter().enumerate().skip(start) {
        page.rows.push(row.clone());
        page.next_after = (index.saturating_add(1) < digest.rows.len()).then_some(index as u64);
        if serde_json::to_vec(&page)?.len() > byte_budget {
            page.rows.pop();
            if page.rows.is_empty() {
                return Err(DigestPageError::OversizedRow {
                    row: index,
                    byte_budget,
                });
            }
            page.next_after = index.checked_sub(1).map(|v| v as u64);
            break;
        }
        if page.rows.len() == usize::from(limit) {
            break;
        }
    }
    Ok(page)
}
