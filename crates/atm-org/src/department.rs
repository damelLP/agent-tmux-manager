//! Owner-supplied department configuration and read-only checks. None of these
//! checks establishes authority or changes the organisation fold.

use crate::{
    budget::validate_budget, ids::validate_name, AgentId, AgentStatus, IdError, OrgState,
    ProjectId, Refusal, RoleSpec, TaskBudget,
};
use atm_core::Money;
use serde::{de, Deserialize, Serialize};
use std::{
    cell::OnceCell,
    collections::{BTreeSet, HashMap},
    fmt,
    num::NonZeroU64,
    ops::Range,
    str::FromStr,
};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DepartmentId(String);

impl DepartmentId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for DepartmentId {
    type Err = IdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        validate_name(text)?;
        Ok(Self(text.into()))
    }
}

impl fmt::Display for DepartmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Department {
    pub id: DepartmentId,
    pub title: String,
    pub goal: Option<String>,
    pub projects: Vec<ProjectId>,
    pub head: Option<AgentId>,
    pub hire_mode: HireMode,
    pub roster: Vec<RosterEntry>,
    pub spend: Option<SpendPolicy>,
}

impl Department {
    #[must_use]
    pub fn enforces(&self, project: &ProjectId) -> bool {
        self.hire_mode == HireMode::Enforce && self.projects.contains(project)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HireMode {
    #[default]
    Report,
    Enforce,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEntry {
    pub seats: SeatPattern,
    pub max: Option<NonZeroU64>,
}

impl RosterEntry {
    #[must_use]
    pub fn matches(&self, seat: &AgentId) -> bool {
        let pattern = &self.seats;
        if seat.project != pattern.project {
            return false;
        }
        if pattern.numbered {
            seat.name.starts_with(&pattern.name)
                && numbered_prefix_lengths(&seat.name).contains(&pattern.name.len())
        } else {
            seat.name == pattern.name
        }
    }

    /// Count recorded seats that are not down, even if this exceeds the cap.
    #[must_use]
    pub fn live(&self, state: &OrgState) -> usize {
        live_seats(state).filter(|seat| self.matches(seat)).count()
    }
}

fn live_seats(state: &OrgState) -> impl Iterator<Item = &AgentId> {
    state
        .agents()
        .filter(|agent| agent.status != AgentStatus::Down)
        .map(|agent| &agent.id)
}

fn numbered_prefix_lengths(name: &str) -> Range<usize> {
    // Any nonempty prefix that leaves at least one trailing ASCII digit.
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    stem.len().max(1)..name.len()
}

/// Index one fold for views that count many roster rows. Duplicate rows use
/// the same count; the row's cap does not affect occupancy.
pub fn live_counts(state: &OrgState) -> impl Fn(&RosterEntry) -> usize + '_ {
    let mut counts = HashMap::new();
    for seat in live_seats(state) {
        counts.insert((&seat.project, seat.name.as_str(), false), 1);
        for len in numbered_prefix_lengths(&seat.name) {
            if let Some(prefix) = seat.name.get(..len) {
                *counts.entry((&seat.project, prefix, true)).or_default() += 1;
            }
        }
    }
    move |entry| {
        let pattern = &entry.seats;
        counts
            .get(&(&pattern.project, pattern.name.as_str(), pattern.numbered))
            .copied()
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterRefusal<'d> {
    NotOnRoster,
    Full {
        pattern: &'d SeatPattern,
        live: usize,
        max: NonZeroU64,
    },
}

impl fmt::Display for RosterRefusal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOnRoster => f.write_str("not on roster"),
            Self::Full { pattern, live, max } => {
                write!(f, "roster full: {live} of {max} for {pattern}")
            }
        }
    }
}

/// Roster rule for a seat in a member project, independent of mode or authority.
#[must_use]
pub fn roster_refusal<'d>(
    dept: &'d Department,
    seat: &AgentId,
    seat_is_live: bool,
    count: &impl Fn(&RosterEntry) -> usize,
) -> Option<RosterRefusal<'d>> {
    let mut reason = RosterRefusal::NotOnRoster;
    for entry in dept.roster.iter().filter(|row| row.matches(seat)) {
        let max = entry.max?;
        let live = count(entry).saturating_sub(usize::from(seat_is_live));
        if (live as u64) < max.get() {
            return None;
        }
        if reason == RosterRefusal::NotOnRoster {
            reason = RosterRefusal::Full {
                pattern: &entry.seats,
                live,
                max,
            };
        }
    }
    Some(reason)
}

const DEFAULT_MODEL: &str = "(default)";

#[must_use]
pub fn role_model(role: &RoleSpec) -> &str {
    role.config.model.as_deref().unwrap_or(DEFAULT_MODEL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartRefusal<'d, 'm> {
    roster: Option<RosterRefusal<'d>>,
    model: Option<&'m str>,
}

impl fmt::Display for StartRefusal<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(roster) = self.roster {
            write!(f, "{roster}")?;
            if self.model.is_some() {
                f.write_str("; ")?;
            }
        }
        if let Some(model) = self.model {
            if model == DEFAULT_MODEL {
                return write!(f, "model {DEFAULT_MODEL} is not allowed");
            }
            f.write_str("model \"")?;
            // Bound the escaped label, never the text used for matching.
            let mut remaining = 192;
            for c in model.chars() {
                let escaped = c.escape_default();
                if escaped.len() > remaining {
                    f.write_str("...")?;
                    break;
                }
                remaining -= escaped.len();
                write!(f, "{escaped}")?;
            }
            f.write_str("\" is not allowed")?;
        }
        Ok(())
    }
}

/// Both rules for one member department. Pass the model from [`role_model`]
/// when a role is known, or `None` to skip the model check.
#[must_use]
pub fn start_refusal<'d, 'm>(
    dept: &'d Department,
    seat: &AgentId,
    seat_is_live: bool,
    model: Option<&'m str>,
    count: &impl Fn(&RosterEntry) -> usize,
) -> Option<StartRefusal<'d, 'm>> {
    let roster = roster_refusal(dept, seat, seat_is_live, count);
    let model = model.filter(|model| {
        dept.spend
            .as_ref()
            .and_then(|spend| spend.models.as_ref())
            .is_some_and(|patterns| !patterns.iter().any(|pattern| pattern.allows(model)))
    });
    (roster.is_some() || model.is_some()).then_some(StartRefusal { roster, model })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HireCheck {
    Owner,
    Uncovered,
    Allowed,
    NeedsApproval { reason: String },
}

/// An attributed owner bypasses hiring policy. Any one enforcing department
/// that passes both rules admits the candidate; this does not grant authority.
#[must_use]
pub fn hire_check(
    departments: &[Department],
    caller: Option<&AgentId>,
    seat: &AgentId,
    role: &RoleSpec,
    state: &OrgState,
) -> HireCheck {
    if caller.is_none() {
        return HireCheck::Owner;
    }
    let is_live = state
        .agent(seat)
        .is_some_and(|agent| agent.status != AgentStatus::Down);
    let counts = OnceCell::new();
    let count = |entry: &RosterEntry| counts.get_or_init(|| live_counts(state))(entry);
    let mut refusals = Vec::new();
    for dept in departments
        .iter()
        .filter(|dept| dept.enforces(&seat.project))
    {
        let Some(reason) = start_refusal(dept, seat, is_live, Some(role_model(role)), &count)
        else {
            return HireCheck::Allowed;
        };
        refusals.push((&dept.id, reason));
    }
    let Some(first) = refusals.first() else {
        return HireCheck::Uncovered;
    };
    let render =
        |(id, reason): &(&DepartmentId, StartRefusal<'_, '_>)| format!("department {id}: {reason}");
    let reason = if refusals.iter().any(|(_, reason)| reason.model.is_some()) {
        refusals.iter().map(render).collect::<Vec<_>>().join("; ")
    } else {
        // Preserve the roster-only refusal: first full row, else first miss.
        render(
            refusals
                .iter()
                .find(|(_, r)| matches!(r.roster, Some(RosterRefusal::Full { .. })))
                .unwrap_or(first),
        )
    };
    HireCheck::NeedsApproval { reason }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatPattern {
    project: ProjectId,
    name: String,
    numbered: bool,
}

impl FromStr for SeatPattern {
    type Err = DepartmentError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || DepartmentError::SeatPattern(text.into());
        let (project, name) = text.split_once('/').ok_or_else(invalid)?;
        let project = project.parse().map_err(|_| invalid())?;
        let prefix = name.strip_suffix('*');
        if let Some(prefix) = prefix {
            if prefix.is_empty() || validate_name(&format!("{prefix}0")).is_err() {
                return Err(invalid());
            }
        } else {
            validate_name(name).map_err(|_| invalid())?;
        }
        Ok(Self {
            project,
            name: prefix.unwrap_or(name).into(),
            numbered: prefix.is_some(),
        })
    }
}

impl fmt::Display for SeatPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let star = if self.numbered { "*" } else { "" };
        write!(f, "{}/{}{star}", self.project, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPattern(String);

impl FromStr for ModelPattern {
    type Err = DepartmentError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() || text.strip_suffix('*').unwrap_or(text).contains('*') {
            return Err(DepartmentError::ModelPattern(text.into()));
        }
        Ok(Self(text.into()))
    }
}

impl ModelPattern {
    #[must_use]
    pub fn allows(&self, model: &str) -> bool {
        match self.0.strip_suffix('*') {
            Some(prefix) => model.starts_with(prefix),
            None => model == self.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendPolicy {
    pub models: Option<Vec<ModelPattern>>,
    pub task_budget: Option<TaskBudget>,
}

/// Per-field provenance for a default budget; not part of the task event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetDefaultSources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<DepartmentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<DepartmentId>,
}

/// Select the strictest present limit for each field in the admitted project.
#[must_use]
pub fn task_budget_default(
    departments: &[Department],
    project: &ProjectId,
) -> Option<(TaskBudget, BudgetDefaultSources)> {
    let budgets = departments
        .iter()
        .filter(|dept| dept.projects.contains(project))
        .filter_map(|dept| Some((dept.spend.as_ref()?.task_budget.as_ref()?, &dept.id)));
    // Minimize (limit, department ID) so ties do not depend on scan order.
    let turns = budgets
        .clone()
        .filter_map(|(budget, id)| Some((budget.turns?, id)))
        .min();
    let cost = budgets
        .filter_map(|(budget, id)| Some((budget.cost?, id)))
        .min();
    if turns.is_none() && cost.is_none() {
        return None;
    }
    Some((
        TaskBudget {
            turns: turns.map(|(limit, _)| limit),
            cost: cost.map(|(limit, _)| limit),
            deadline: None,
        },
        BudgetDefaultSources {
            turns: turns.map(|(_, id)| id.clone()),
            cost: cost.map(|(_, id)| id.clone()),
        },
    ))
}

#[derive(Debug, Error)]
pub enum DepartmentError {
    #[error("id: {0}")]
    Id(#[from] IdError),
    #[error("{}", .source.message().lines().next().unwrap_or_default())]
    Toml {
        line: Option<usize>,
        #[source]
        source: toml::de::Error,
    },
    #[error("projects: projects must not be empty")]
    EmptyProjects,
    #[error("projects: duplicate project {0}")]
    DuplicateProject(ProjectId),
    #[error("roster.seats: invalid seat pattern {0:?}")]
    SeatPattern(String),
    #[error("spend.models: invalid model pattern {0:?}")]
    ModelPattern(String),
    #[error("spend.task_budget.cost.microdollars: {0}")]
    Budget(#[from] Refusal),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DepartmentProblem {
    #[error("projects: project {project} is not registered")]
    UnknownProject {
        department: DepartmentId,
        project: ProjectId,
    },
    #[error("roster.seats: {seats} is outside the department's projects")]
    RosterOutsideProjects {
        department: DepartmentId,
        seats: SeatPattern,
    },
    #[error("head: {head} is not in a registered project")]
    InvalidHead {
        department: DepartmentId,
        head: AgentId,
    },
}

impl DepartmentProblem {
    #[must_use]
    pub fn department_id(&self) -> &DepartmentId {
        match self {
            Self::UnknownProject { department, .. }
            | Self::RosterOutsideProjects { department, .. }
            | Self::InvalidHead { department, .. } => department,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDepartment {
    title: String,
    goal: Option<String>,
    projects: Vec<ProjectId>,
    head: Option<AgentId>,
    #[serde(default)]
    hire: RawHire,
    #[serde(default)]
    roster: Vec<RawRosterItem>,
    spend: Option<RawSpend>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHire {
    #[serde(default)]
    mode: HireMode,
}

struct RawRosterItem(RawRosterEntry);

impl<'de> Deserialize<'de> for RawRosterItem {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RosterVisitor;

        impl<'de> de::Visitor<'de> for RosterVisitor {
            type Value = RawRosterItem;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a seat pattern string or a roster table")
            }

            fn visit_str<E: de::Error>(self, seats: &str) -> Result<Self::Value, E> {
                self.visit_string(seats.to_owned())
            }

            fn visit_string<E: de::Error>(self, seats: String) -> Result<Self::Value, E> {
                Ok(RawRosterItem(RawRosterEntry { seats, max: None }))
            }

            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                // Forward the original map so its key and value spans survive.
                RawRosterEntry::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(RawRosterItem)
            }
        }

        deserializer.deserialize_any(RosterVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRosterEntry {
    seats: String,
    max: Option<NonZeroU64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpend {
    models: Option<Vec<String>>,
    task_budget: Option<RawBudget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    turns: Option<u32>,
    cost: Option<RawCost>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCost {
    microdollars: i64,
}

impl TryFrom<RawSpend> for SpendPolicy {
    type Error = DepartmentError;

    fn try_from(raw: RawSpend) -> Result<Self, Self::Error> {
        let task_budget = raw.task_budget.map(|budget| TaskBudget {
            turns: budget.turns,
            cost: budget
                .cost
                .map(|cost| Money::from_microdollars(cost.microdollars)),
            deadline: None,
        });
        validate_budget(task_budget.as_ref())?;
        let models = raw
            .models
            .map(|models| {
                models
                    .iter()
                    .map(|model| model.parse())
                    .collect::<Result<_, _>>()
            })
            .transpose()?;
        Ok(Self {
            models,
            task_budget,
        })
    }
}

pub fn parse_department(id: &str, text: &str) -> Result<Department, DepartmentError> {
    let id = id.parse()?;
    let raw: RawDepartment = toml::from_str(text).map_err(|source: toml::de::Error| {
        let line = source.span().map(|span| {
            1 + text
                .bytes()
                .take(span.start)
                .filter(|b| *b == b'\n')
                .count()
        });
        DepartmentError::Toml { line, source }
    })?;
    if raw.projects.is_empty() {
        return Err(DepartmentError::EmptyProjects);
    }
    let mut seen = BTreeSet::new();
    for project in &raw.projects {
        if !seen.insert(project) {
            return Err(DepartmentError::DuplicateProject(project.clone()));
        }
    }
    let roster = raw
        .roster
        .into_iter()
        .map(|RawRosterItem(RawRosterEntry { seats, max })| {
            seats.parse().map(|seats| RosterEntry { seats, max })
        })
        .collect::<Result<_, _>>()?;
    Ok(Department {
        id,
        title: raw.title,
        goal: raw.goal,
        projects: raw.projects,
        head: raw.head,
        hire_mode: raw.hire.mode,
        roster,
        spend: raw.spend.map(SpendPolicy::try_from).transpose()?,
    })
}

/// Check references without discarding a parsed department or requiring a live head.
#[must_use]
pub fn validate(
    departments: &[Department],
    registered: &BTreeSet<ProjectId>,
) -> Vec<DepartmentProblem> {
    let mut problems = Vec::new();
    for dept in departments {
        for project in &dept.projects {
            if !registered.contains(project) {
                problems.push(DepartmentProblem::UnknownProject {
                    department: dept.id.clone(),
                    project: project.clone(),
                });
            }
        }
        for entry in &dept.roster {
            if !dept.projects.contains(&entry.seats.project) {
                problems.push(DepartmentProblem::RosterOutsideProjects {
                    department: dept.id.clone(),
                    seats: entry.seats.clone(),
                });
            }
        }
        if let Some(head) = dept
            .head
            .as_ref()
            .filter(|head| !registered.contains(&head.project))
        {
            problems.push(DepartmentProblem::InvalidHead {
                department: dept.id.clone(),
                head: head.clone(),
            });
        }
    }
    problems
}
