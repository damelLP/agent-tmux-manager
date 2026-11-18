//! Report inputs and read-only projections; no persisted schema lives here.
use crate::{Address, AgentId, ProjectId, TaskCost, TaskId, TaskKind, TaskMeasure, TaskState};
use atm_core::Money;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportParams {
    pub project_id: Option<ProjectId>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    #[serde(default = "bucket")]
    pub bucket: u16,
    pub after: Option<TaskId>,
    #[serde(default = "limit")]
    pub limit: u16,
}
fn bucket() -> u16 {
    5
}
fn limit() -> u16 {
    100
}

#[derive(Debug, Clone)]
pub enum ReportVisibility {
    All,
    Reports {
        reader: AgentId,
        agents: BTreeSet<AgentId>,
    },
}
impl ReportVisibility {
    pub(super) fn contains(&self, id: &AgentId) -> bool {
        match self {
            Self::All => true,
            Self::Reports { reader, agents } => id == reader || agents.contains(id),
        }
    }
    pub(super) fn address(&self, address: &Address) -> bool {
        match address {
            Address::Agent(id) => self.contains(id),
            _ => matches!(self, Self::All),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub now: DateTime<Utc>,
    pub scope: ReportScope,
    pub tasks: Vec<TaskReport>,
    pub projects: Vec<ProjectReport>,
    pub next_after: Option<TaskId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportScope {
    pub project_id: Option<ProjectId>,
    pub since: Option<DateTime<Utc>>,
    pub until: DateTime<Utc>,
    pub reader: Address,
    pub sees: ReportSees,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportSees {
    All,
    Reports,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted,
    Cancelled,
    Open,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerStepIns {
    pub total: u64,
    pub verdict: u64,
    pub redirect: u64,
    pub answer: u64,
    pub message: u64,
    pub redo: u64,
}
impl OwnerStepIns {
    pub(super) fn add(&mut self, other: &Self) {
        self.total = self.total.saturating_add(other.total);
        self.verdict = self.verdict.saturating_add(other.verdict);
        self.redirect = self.redirect.saturating_add(other.redirect);
        self.answer = self.answer.saturating_add(other.answer);
        self.message = self.message.saturating_add(other.message);
        self.redo = self.redo.saturating_add(other.redo);
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSource {
    pub label: CostLabel,
    pub harnesses: Vec<String>,
    pub models: Vec<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostLabel {
    HarnessReported,
    Unknown,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OverBudget {
    pub turns: Option<u64>,
    #[serde(default, with = "crate::event::optional_money")]
    pub cost: Option<Money>,
    pub deadline_seconds: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReport {
    pub task_id: TaskId,
    pub title: String,
    pub kind: TaskKind,
    pub measure: TaskMeasure,
    pub project_id: Option<ProjectId>,
    pub agent_id: Option<AgentId>,
    pub state: TaskState,
    pub outcome: Outcome,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub usage: TaskCost,
    pub submissions: u64,
    pub reminders: u64,
    pub flags: u64,
    pub redone: u64,
    pub owner_step_ins: OwnerStepIns,
    pub context_percent_at_submit: Option<u8>,
    pub cost_source: CostSource,
    pub over_budget: Option<OverBudget>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutcomeCounts {
    pub accepted: u64,
    pub cancelled: u64,
    pub open: u64,
    pub unknown: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionCounts {
    pub taken_for_owner: u64,
    pub agent_decided: u64,
    #[serde(default)]
    pub relayed: u64,
    pub overruled: u64,
    pub tasks_redone: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectReport {
    pub project_id: Option<ProjectId>,
    pub tasks: OutcomeCounts,
    pub owner_step_ins: OwnerStepIns,
    pub tasks_with_step_ins: u64,
    #[serde(default, with = "crate::event::optional_money")]
    pub cost: Option<Money>,
    pub cost_unknown_tasks: u64,
    pub over_budget: Option<OverBudget>,
    pub decisions: DecisionCounts,
    pub trend: Trend,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trend {
    pub bucket: u16,
    pub buckets: Vec<TrendBucket>,
    pub direction: TrendDirection,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendBucket {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub n: u16,
    pub turns: Option<u32>,
    pub peak_context_percent: Option<u8>,
    pub review_rounds: Option<u32>,
    pub changes_requested: Option<u32>,
    #[serde(default, with = "crate::event::optional_money")]
    pub cost: Option<Money>,
    pub cost_known: u16,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Flat,
    #[default]
    None,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrendDirection {
    pub turns: Direction,
    pub peak_context_percent: Direction,
    pub review_rounds: Direction,
    pub changes_requested: Direction,
    pub cost: Direction,
}
