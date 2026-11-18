//! Pure Work grouping, badge projection and refresh state. No I/O or clock reads.

use atm_core::{SessionView, TreeNode, TreeNodeId};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

macro_rules! id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
    };
}
id!(WindowId);
id!(PaneId);
id!(SeatId);
id!(TaskId);

pub const NO_WINDOW: &str = "__no_window__";
#[derive(Debug, Clone, Deserialize)]
pub struct WorkWindow {
    pub id: WindowId,
    pub name: String,
    pub links: Vec<WindowLink>,
    pub panes: Vec<PaneId>,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub struct WindowLink {
    pub session: String,
    pub index: u32,
}
impl WorkWindow {
    fn link(&self) -> Option<&WindowLink> {
        self.links.iter().min()
    }
    fn label(&self) -> String {
        match self.link() {
            Some(link) => format!("{} {}", link.index, self.name).trim_end().into(),
            None => self.name.clone(),
        }
    }
}
fn agents(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    nodes
        .into_iter()
        .flat_map(|node| match node {
            TreeNode::Agent { .. } => vec![node],
            TreeNode::Project { children, .. }
            | TreeNode::Worktree { children, .. }
            | TreeNode::Team { children, .. }
            | TreeNode::Window { children, .. } => agents(children),
        })
        .collect()
}
fn restore_parents(nodes: &mut [TreeNode], originals: &BTreeMap<&str, &SessionView>) {
    for node in nodes {
        if let TreeNode::Agent { session, subagents } = node {
            if let Some(original) = originals.get(session.id.as_str()) {
                session
                    .parent_session_id
                    .clone_from(&original.parent_session_id);
            }
            restore_parents(subagents, originals);
        }
    }
}
/// Reuse Sessions' tree builder for agent nesting/spawn order, not its git groups.
#[must_use]
pub fn build_work_tree(sessions: &[SessionView], windows: &[WorkWindow]) -> Vec<TreeNode> {
    let originals: BTreeMap<_, _> = sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let panes: BTreeMap<_, _> = windows
        .iter()
        .flat_map(|w| w.panes.iter().map(move |p| (p.0.as_str(), &w.id)))
        .collect();
    let mut groups = BTreeMap::<WindowId, Vec<SessionView>>::new();
    for session in sessions {
        let mut current = Some(session);
        let mut visited = BTreeSet::new();
        let mut group = WindowId(NO_WINDOW.into());
        while let Some(row) = current.filter(|row| visited.insert(row.id.as_str())) {
            if let Some(pane) = &row.tmux_pane {
                if let Some(id) = panes.get(pane.as_str()) {
                    group = (*id).clone();
                }
                break;
            }
            current = row
                .parent_session_id
                .as_ref()
                .and_then(|id| originals.get(id.as_str()).copied());
        }
        groups.entry(group).or_default().push(session.clone());
    }
    let mut ordered: Vec<_> = windows.iter().collect();
    ordered.sort_by(|a, b| a.link().cmp(&b.link()).then(a.id.cmp(&b.id)));
    let mut tree = Vec::new();
    let definitions = ordered
        .into_iter()
        .map(|w| (w.id.clone(), w.label()))
        .chain(std::iter::once((
            WindowId(NO_WINDOW.into()),
            "(no window)".into(),
        )));
    for (id, label) in definitions {
        let Some(mut group) = groups.remove(&id) else {
            continue;
        };
        let mut built = atm_core::build_tree(&group);
        let emitted = atm_core::all_node_ids(&built);
        // Invalid parent cycles are unreachable from a root in build_tree.
        // Promote their component rather than hiding it; restore row data below.
        if group
            .iter()
            .any(|s| !emitted.contains(&TreeNodeId::Agent(s.id.clone())))
        {
            for row in &mut group {
                if !emitted.contains(&TreeNodeId::Agent(row.id.clone())) {
                    row.parent_session_id = None;
                }
            }
            built = atm_core::build_tree(&group);
        }
        let mut children = agents(built);
        children.sort_by(|a, b| match (a, b) {
            (TreeNode::Agent { session: a, .. }, TreeNode::Agent { session: b, .. }) => {
                b.started_at.cmp(&a.started_at)
            }
            _ => std::cmp::Ordering::Equal,
        });
        restore_parents(&mut children, &originals);
        tree.push(TreeNode::Window {
            id: id.0,
            label,
            children,
        });
    }
    tree
}

/// A read projection, not the CLI's display schema. Unknown additive fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkOrg {
    #[serde(default)]
    pub seq: Option<u64>,
    pub projects: Vec<WorkProject>,
    pub needs_owner: WorkNeedsOwner,
}
#[derive(Debug, Clone, Deserialize)]
pub struct WorkProject {
    pub seats: Vec<WorkSeat>,
    pub tasks: Vec<WorkTask>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct WorkSeat {
    pub agent_id: SeatId,
    pub pane: Option<PaneId>,
    pub tasks: Vec<TaskId>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct WorkTask {
    pub task_id: TaskId,
    pub title: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct WorkNeedsOwner {
    pub items: Vec<WorkNeed>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct WorkNeed {
    pub kind: String,
    pub agent_id: Option<SeatId>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskMark {
    pub id: TaskId,
    pub title: Option<String>,
}
#[derive(Debug, Clone)]
pub struct PaneMarks {
    pub agent_id: SeatId,
    pub name: String,
    pub first_task: Option<TaskMark>,
    pub tasks_open: usize,
    pub decisions_waiting: usize,
    pub attention: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowMarks {
    pub tasks_open: usize,
    pub decisions_waiting: usize,
    pub attention: bool,
}
#[derive(Debug, Clone, Default)]
pub struct WorkMarks {
    pub by_pane: BTreeMap<PaneId, PaneMarks>,
    pub by_window: BTreeMap<WindowId, WindowMarks>,
}

#[must_use]
pub fn work_marks(org: &WorkOrg, windows: &[WorkWindow]) -> WorkMarks {
    let tasks: BTreeMap<_, _> = org
        .projects
        .iter()
        .flat_map(|p| p.tasks.iter())
        .map(|t| (&t.task_id, &t.title))
        .collect();
    let mut marks = WorkMarks::default();
    for seat in org.projects.iter().flat_map(|p| &p.seats) {
        let Some(pane) = &seat.pane else { continue };
        let needs: Vec<_> = org
            .needs_owner
            .items
            .iter()
            .filter(|item| item.agent_id.as_ref() == Some(&seat.agent_id))
            .collect();
        marks
            .by_pane
            .entry(pane.clone())
            .or_insert_with(|| PaneMarks {
                agent_id: seat.agent_id.clone(),
                name: seat
                    .agent_id
                    .0
                    .rsplit('/')
                    .next()
                    .unwrap_or(&seat.agent_id.0)
                    .into(),
                first_task: seat.tasks.first().map(|id| TaskMark {
                    id: id.clone(),
                    title: tasks.get(id).map(|title| (*title).clone()),
                }),
                tasks_open: seat.tasks.len(),
                decisions_waiting: needs
                    .iter()
                    .filter(|item| {
                        matches!(
                            item.kind.as_str(),
                            "decision_pending" | "decision_provisional"
                        )
                    })
                    .count(),
                attention: needs.iter().any(|item| {
                    matches!(
                        item.kind.as_str(),
                        "seat_stuck" | "seat_attention" | "delivery_failed" | "line_broken"
                    )
                }),
            });
    }
    for window in windows {
        let mut value = WindowMarks::default();
        for pane in window.panes.iter().collect::<BTreeSet<_>>() {
            if let Some(mark) = marks.by_pane.get(pane) {
                value.tasks_open += mark.tasks_open;
                value.decisions_waiting += mark.decisions_waiting;
                value.attention |= mark.attention;
            }
        }
        marks.by_window.insert(window.id.clone(), value);
    }
    marks
}

/// Only the fields consumed from session.windows; timestamps use the local injected clock.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkWindows {
    pub state: String,
    pub windows: Vec<WorkWindow>,
}
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct WorkFailure(pub String);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation(u64);
#[derive(Debug)]
pub enum WorkInput {
    Shown,
    Hidden,
    Capabilities {
        methods: Vec<String>,
        watch: bool,
    },
    PanesChanged,
    WindowsReply {
        generation: Generation,
        result: Result<WorkWindows, WorkFailure>,
    },
    OrgReply {
        generation: Generation,
        result: Result<WorkOrg, WorkFailure>,
    },
    WatchEvent {
        seq: u64,
        kind: String,
    },
    WatchFailed,
    Refresh,
    Wake,
}
/// The driver replaces its one timer after each reduction: no WakeAt means disarm it.
/// Return a generation-tagged terminal reply for every read, including after Hidden.
/// Watch/Unwatch must be arbitrated with the decision controller, not executed blindly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkEffect {
    RequestWindows(Generation),
    RequestOrg(Generation),
    Watch { cursor: u64, topics: Vec<String> },
    Unwatch,
    WakeAt(Duration),
}
#[derive(Debug, Clone)]
struct Cached<T> {
    value: T,
    at: Duration,
}
#[derive(Debug, Clone)]
struct Read {
    generation: Generation,
    valid: bool,
}
#[derive(Debug, Clone, Default)]
struct Poll {
    flight: Option<Read>,
    dirty_at: Option<Duration>,
    due: Option<Duration>,
    last_request: Option<Duration>,
}
impl Poll {
    fn trigger(&mut self, at: Duration) {
        self.dirty_at = Some(self.dirty_at.map_or(at, |old| old.min(at)));
    }
    fn suspend(&mut self) {
        self.dirty_at = None;
        self.due = None;
        if let Some(read) = &mut self.flight {
            read.valid = false;
        }
    }
    fn tick(&mut self, now: Duration) {
        if self.due.is_some_and(|due| due <= now) {
            self.due = None;
            self.trigger(now);
        }
    }
    fn finish(&mut self, generation: Generation) -> Option<bool> {
        if self
            .flight
            .as_ref()
            .is_some_and(|read| read.generation == generation)
        {
            self.flight.take().map(|read| read.valid)
        } else {
            None
        }
    }
    fn start(&mut self, now: Duration, period: Duration, counter: &mut u64) -> Option<Generation> {
        if self.flight.is_some() || !self.dirty_at.is_some_and(|at| at <= now) {
            return None;
        }
        self.dirty_at = None;
        let Some(next) = counter.checked_add(1) else {
            self.due = None;
            return None;
        };
        *counter = next;
        let generation = Generation(next);
        self.flight = Some(Read {
            generation,
            valid: true,
        });
        self.last_request = Some(now);
        self.due = Some(now.saturating_add(period));
        Some(generation)
    }
    fn deadline(&self) -> Option<Duration> {
        self.due
            .into_iter()
            .chain(self.dirty_at.filter(|_| self.flight.is_none()))
            .min()
    }
}
/// Persistent caches and request bookkeeping only. Every time value is supplied by the caller.
#[derive(Debug, Clone)]
pub struct WorkState {
    visible: bool,
    known: bool,
    windows_supported: bool,
    org_supported: bool,
    watch_supported: bool,
    watching: bool,
    watch_failed: bool,
    counter: u64,
    window_poll: Poll,
    org_poll: Poll,
    windows: Option<Cached<Vec<WorkWindow>>>,
    org: Option<Cached<WorkOrg>>,
}
impl Default for WorkState {
    fn default() -> Self {
        Self {
            visible: false,
            known: false,
            windows_supported: true,
            org_supported: true,
            watch_supported: false,
            watching: false,
            watch_failed: false,
            counter: 0,
            window_poll: Poll::default(),
            org_poll: Poll::default(),
            windows: None,
            org: None,
        }
    }
}
impl WorkState {
    #[must_use]
    pub fn needs_newer_daemon(&self) -> bool {
        self.known && !self.windows_supported
    }
    #[must_use]
    pub fn no_org_data(&self) -> bool {
        self.known && !self.org_supported
    }
    #[must_use]
    pub fn windows(&self) -> &[WorkWindow] {
        self.windows
            .as_ref()
            .map_or(&[], |cache| cache.value.as_slice())
    }
    #[must_use]
    pub fn org(&self) -> Option<&WorkOrg> {
        self.org
            .as_ref()
            .filter(|_| self.org_supported)
            .map(|cache| &cache.value)
    }
    /// Age of the oldest source past its own freshness threshold, never failed-reply age.
    #[must_use]
    pub fn stale_age(&self, now: Duration) -> Option<Duration> {
        let windows = self
            .windows
            .as_ref()
            .map(|c| now.saturating_sub(c.at))
            .filter(|age| *age > Duration::from_secs(10));
        let org = self
            .org
            .as_ref()
            .filter(|_| self.org_supported)
            .map(|c| now.saturating_sub(c.at))
            .filter(|age| *age > Duration::from_secs(180));
        windows.into_iter().chain(org).max()
    }
    fn unwatch(&mut self, effects: &mut Vec<WorkEffect>) {
        if self.watching {
            effects.push(WorkEffect::Unwatch);
            self.watching = false;
        }
    }
    fn watch(&mut self, effects: &mut Vec<WorkEffect>) {
        if self.visible
            && self.windows_supported
            && self.org_supported
            && self.watch_supported
            && !self.watch_failed
            && !self.watching
        {
            if let Some(cursor) = self.org.as_ref().and_then(|c| c.value.seq) {
                effects.push(WorkEffect::Watch {
                    cursor,
                    topics: ["task.", "decision.", "agent.", "msg.delivery_failed"]
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                });
                self.watching = true;
            }
        }
    }
    #[must_use]
    pub fn reduce(&mut self, input: WorkInput, now: Duration) -> Vec<WorkEffect> {
        let mut effects = Vec::new();
        match input {
            WorkInput::Shown if !self.visible => {
                self.visible = true;
                self.watch_failed = false;
                self.window_poll.trigger(now);
                self.org_poll.trigger(now);
            }
            WorkInput::Hidden => {
                self.visible = false;
                self.window_poll.suspend();
                self.org_poll.suspend();
                self.unwatch(&mut effects);
            }
            WorkInput::Capabilities { methods, watch } => {
                let reconnect = self.known;
                self.known = true;
                self.windows_supported = methods.iter().any(|m| m == "session.windows");
                self.org_supported = methods.iter().any(|m| m == "org.tree");
                self.watch_supported = watch;
                self.watch_failed = false;
                if reconnect || !self.windows_supported {
                    self.window_poll.suspend();
                    self.org_poll.suspend();
                    self.unwatch(&mut effects);
                } else if !self.org_supported {
                    self.org_poll.suspend();
                    self.unwatch(&mut effects);
                }
                if self.visible && self.windows_supported {
                    if reconnect || (self.window_poll.flight.is_none() && self.windows.is_none()) {
                        self.window_poll.trigger(now);
                    }
                    if self.org_supported
                        && (reconnect || (self.org_poll.flight.is_none() && self.org.is_none()))
                    {
                        self.org_poll.trigger(now);
                    }
                    if !reconnect && self.org_poll.flight.is_none() {
                        self.watch(&mut effects);
                    }
                }
            }
            WorkInput::Refresh if self.visible && self.windows_supported => {
                self.watch_failed = false;
                self.window_poll.trigger(now);
                self.org_poll.trigger(now);
            }
            WorkInput::PanesChanged if self.visible && self.windows_supported => {
                self.window_poll.trigger(now)
            }
            WorkInput::WindowsReply { generation, result } => {
                if self.window_poll.finish(generation) == Some(true)
                    && self.visible
                    && self.windows_supported
                {
                    if let Ok(value) = result {
                        if value.state == "ok" {
                            self.windows = Some(Cached {
                                value: value.windows,
                                at: now,
                            });
                        }
                    }
                }
            }
            WorkInput::OrgReply { generation, result } => {
                if self.org_poll.finish(generation) == Some(true)
                    && self.visible
                    && self.windows_supported
                    && self.org_supported
                {
                    if let Ok(value) = result {
                        if value.seq.is_none() {
                            self.unwatch(&mut effects);
                        }
                        self.org = Some(Cached { value, at: now });
                        self.watch(&mut effects);
                    }
                }
            }
            WorkInput::WatchEvent { seq, kind }
                if self.visible && self.windows_supported && self.org_supported =>
            {
                if ["task.", "decision.", "agent.", "msg.delivery_failed"]
                    .iter()
                    .any(|prefix| kind.starts_with(prefix))
                    && self
                        .org
                        .as_ref()
                        .and_then(|c| c.value.seq)
                        .is_none_or(|old| seq > old)
                {
                    let earliest = self
                        .org_poll
                        .last_request
                        .map_or(now, |at| at.saturating_add(Duration::from_secs(5)))
                        .max(now);
                    self.org_poll.trigger(earliest);
                }
            }
            WorkInput::WatchFailed => {
                self.watch_failed = true;
                self.unwatch(&mut effects);
            }
            _ => {}
        }
        if self.visible && self.windows_supported {
            self.window_poll.tick(now);
            if let Some(id) = self
                .window_poll
                .start(now, Duration::from_secs(3), &mut self.counter)
            {
                effects.push(WorkEffect::RequestWindows(id));
            }
            if self.org_supported {
                self.org_poll.tick(now);
                if let Some(id) =
                    self.org_poll
                        .start(now, Duration::from_secs(60), &mut self.counter)
                {
                    effects.push(WorkEffect::RequestOrg(id));
                }
            }
            let deadline = self
                .window_poll
                .deadline()
                .into_iter()
                .chain(self.org_poll.deadline().filter(|_| self.org_supported))
                .min();
            if let Some(at) = deadline {
                effects.push(WorkEffect::WakeAt(at));
            }
        }
        effects
    }
}

#[cfg(test)]
#[path = "work_tests.rs"]
mod tests;
