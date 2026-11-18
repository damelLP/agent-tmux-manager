//! Application state machine for the ATM TUI.
//!
//! This module defines the core state model for the TUI, including
//! connection state tracking and session management.
//!
//! All code follows the panic-free policy: no `.unwrap()`, `.expect()`,
//! `panic!()`, `unreachable!()`, `todo!()`, or direct indexing `[i]`.

use crate::{
    beads::BeadsTask,
    work::{
        build_work_threads, build_work_tree, WorkEffect, WorkGrouping, WorkInput, WorkState,
        NO_WINDOW,
    },
};
use atm_core::{
    all_node_ids, build_tree, flatten_tree, SessionId, SessionView, TreeNode, TreeNodeId, TreeRow,
    TreeRowKind,
};
use chrono::{DateTime, Utc};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

/// Work is the initial view; older daemons initially fall back to Sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AppView {
    Sessions,
    #[default]
    Work,
}

// ============================================================================
// Application State
// ============================================================================

/// Connection state of the TUI to the daemon.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AppState {
    /// Connected to daemon and receiving updates.
    Connected,

    /// Lost connection, attempting reconnect.
    Disconnected {
        /// When the connection was lost.
        since: DateTime<Utc>,
        /// Number of reconnection attempts.
        retry_count: u32,
        /// Why the daemon rejected the last connection, if it did.
        reason: Option<String>,
    },

    /// Initial connection in progress.
    #[default]
    Connecting,
}

// ============================================================================
// Application
// ============================================================================

/// Core application state for the ATM TUI.
///
/// Manages connection state, session data, and UI state for the
/// htop-style management interface.
#[derive(Debug, Clone)]
pub struct App {
    /// Current connection state to the daemon.
    pub state: AppState,

    /// Current grouping, independent of the session data and selection identity.
    pub view: AppView,
    /// Only an untouched startup choice may fall back on the first capabilities.
    initial_view_pending: bool,
    /// Pure Work caches and generation bookkeeping; only the binary executes effects.
    pub work: WorkState,
    /// Reporting threads by default; gw exposes the physical window view.
    pub work_grouping: WorkGrouping,
    work_window_count: usize,
    /// Monotonic frame time injected by the event loop for Work freshness.
    pub work_now: Duration,

    /// All active sessions indexed by their ID.
    pub sessions: HashMap<SessionId, SessionView>,

    /// Index of the currently selected session in the sorted list.
    pub selected_index: usize,

    /// Flag indicating the application should quit.
    pub should_quit: bool,

    /// Wall-clock time of the frame being drawn. The event loop sets it
    /// before each draw, so rendering never reads the clock itself.
    pub now: DateTime<Utc>,

    /// Whether blinking status icons are currently visible.
    /// Toggles every 500ms (5 ticks at 100ms tick rate).
    pub blink_visible: bool,

    /// Internal tick counter for blink timing.
    tick_count: u32,

    /// Pick mode: exit after jumping to a session (fzf-style).
    pub pick_mode: bool,

    /// Whether the help popup is currently visible.
    pub show_help: bool,

    /// Lines the help popup is scrolled down by. Reset when help toggles.
    pub help_scroll: u16,

    /// Set of expanded tree node IDs.
    pub expanded: HashSet<TreeNodeId>,

    /// Cached tree structure (rebuilt on session changes).
    tree: Vec<TreeNode>,

    /// Flattened tree rows for rendering/navigation (rebuilt on session changes or expand/collapse).
    pub tree_rows: Vec<TreeRow>,

    /// Captured terminal output from the selected agent's tmux pane.
    pub captured_output: Vec<String>,

    /// The pane ID currently being captured (to detect selection changes).
    pub capture_pane_id: Option<String>,

    /// Latest in-progress Beads task read for each project directory,
    /// filled by a background read so rendering never touches the disk.
    beads_tasks: HashMap<String, Option<BeadsTask>>,

    /// If set, only show sessions whose tmux pane belongs to this tmux session.
    pub tmux_session_filter: Option<String>,

    /// Pane IDs that belong to the filtered tmux session (populated by filter task).
    pub filter_pane_ids: HashSet<String>,

    /// Panes tagged as coordinators on the TUI's tmux server.
    pub coordinator_panes: HashSet<String>,

    /// Server-projected named panes with a visible waiting owner decision.
    pub decision_panes: HashSet<String>,

    /// Compact mode: vertical layout optimized for narrow sidebar panes.
    pub compact: bool,

    /// Free-text filter query (empty = no filter). Applied live while typing.
    pub filter_query: String,

    /// Whether the `/` search prompt is open and capturing keystrokes.
    pub search_active: bool,

    /// Sessions matching `filter_query`, or `None` when no filter is active.
    /// Visible sessions outside this set are shown only as context.
    pub filter_matches: Option<HashSet<SessionId>>,

    /// Fold state saved when a filter starts, restored when it ends.
    /// `Some` exactly while a filter is active; `Some(None)` when the filter
    /// started before any tree was built, so there is no fold state to keep.
    pre_filter_expanded: Option<Option<HashSet<TreeNodeId>>>,

    /// Inactive view folds; None means it has never been built, not collapsed-all.
    inactive_expanded: Option<HashSet<TreeNodeId>>,
    inactive_pre_filter_expanded: Option<Option<HashSet<TreeNodeId>>>,
    inactive_filter_query: String,

    /// Row selected before the last rebuild that had one, so ending a filter
    /// keeps the cursor even when the filtered tree was left with no rows.
    last_selected_node: Option<TreeNodeId>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Creates a new App in the Connecting state.
    pub fn new() -> Self {
        Self {
            state: AppState::Connecting,
            view: AppView::Work,
            initial_view_pending: true,
            work: WorkState::default(),
            work_grouping: WorkGrouping::default(),
            work_window_count: 0,
            work_now: Duration::ZERO,
            sessions: HashMap::new(),
            selected_index: 0,
            should_quit: false,
            now: Utc::now(),
            blink_visible: true,
            tick_count: 0,
            pick_mode: false,
            show_help: false,
            help_scroll: 0,
            expanded: HashSet::new(),
            tree: Vec::new(),
            tree_rows: Vec::new(),
            captured_output: Vec::new(),
            capture_pane_id: None,
            beads_tasks: HashMap::new(),
            tmux_session_filter: None,
            filter_pane_ids: HashSet::new(),
            coordinator_panes: HashSet::new(),
            decision_panes: HashSet::new(),
            compact: false,
            filter_query: String::new(),
            search_active: false,
            filter_matches: None,
            pre_filter_expanded: None,
            inactive_expanded: None,
            inactive_pre_filter_expanded: None,
            inactive_filter_query: String::new(),
            last_selected_node: None,
        }
    }

    /// Selects a one-based tab (Work, Sessions); invalid tabs leave effects and state alone.
    #[must_use]
    pub fn select_tab(&mut self, one_based: usize, now: Duration) -> Option<Vec<WorkEffect>> {
        let view = match one_based {
            1 => AppView::Work,
            2 => AppView::Sessions,
            _ => return None,
        };
        self.initial_view_pending = false;
        Some(if self.view != view {
            self.toggle_work_view(now)
        } else {
            self.reduce_work(
                match view {
                    AppView::Work => WorkInput::Shown,
                    AppView::Sessions => WorkInput::Hidden,
                },
                now,
            )
        })
    }

    /// Switches grouping without changing a selected agent's identity or the other view's folds.
    #[must_use]
    pub fn toggle_work_view(&mut self, now: Duration) -> Vec<WorkEffect> {
        self.initial_view_pending = false;
        let selected = self
            .selected_session()
            .map(|session| TreeNodeId::Agent(session.id.clone()));
        let query_changed = self.inactive_filter_query != self.filter_query;
        self.inactive_filter_query.clone_from(&self.filter_query);
        let saved = self
            .inactive_expanded
            .replace(std::mem::take(&mut self.expanded));
        std::mem::swap(
            &mut self.pre_filter_expanded,
            &mut self.inactive_pre_filter_expanded,
        );
        self.view = match self.view {
            AppView::Sessions => AppView::Work,
            AppView::Work => AppView::Sessions,
        };
        if !self.filter_query.is_empty() && self.pre_filter_expanded.is_none() {
            self.pre_filter_expanded = Some(saved.clone());
        }
        self.rebuild_tree();
        self.expanded = saved.unwrap_or_else(|| all_node_ids(&self.tree));
        if query_changed && !self.filter_query.is_empty() {
            self.expanded.extend(all_node_ids(&self.tree));
        }
        self.restore_node(selected.as_ref());
        self.reduce_work(
            match self.view {
                AppView::Work => WorkInput::Shown,
                AppView::Sessions => WorkInput::Hidden,
            },
            now,
        )
    }

    /// Opens the new Work grouping without disturbing tab, filter, pane identity or read ownership.
    #[must_use]
    pub fn toggle_work_grouping(&mut self, now: Duration) -> Option<Vec<WorkEffect>> {
        if self.view != AppView::Work {
            return None;
        }
        self.initial_view_pending = false;
        let selected = self
            .selected_session()
            .map(|s| TreeNodeId::Agent(s.id.clone()));
        self.work_grouping = match self.work_grouping {
            WorkGrouping::Threads => WorkGrouping::Windows,
            WorkGrouping::Windows => WorkGrouping::Threads,
        };
        self.rebuild_tree();
        self.expanded = all_node_ids(&self.tree);
        if !self.filter_query.is_empty() {
            self.pre_filter_expanded = Some(Some(self.expanded.clone()));
        }
        self.restore_node(selected.as_ref());
        Some(self.reduce_work(WorkInput::Shown, now))
    }

    /// Reduces one terminal reply/event without rebuilding Sessions for hidden Work I/O.
    #[must_use]
    pub fn reduce_work(&mut self, input: WorkInput, now: Duration) -> Vec<WorkEffect> {
        self.work_now = now;
        let rebuild = self.view == AppView::Work
            && matches!(
                &input,
                WorkInput::WindowsReply { .. }
                    | WorkInput::OrgReply { .. }
                    | WorkInput::Capabilities { .. }
            );
        let initial = self.initial_view_pending && matches!(&input, WorkInput::Capabilities { .. });
        if initial {
            self.initial_view_pending = false;
        }
        let mut effects = self.work.reduce(input, now);
        if initial && self.view == AppView::Work {
            if self.work.needs_newer_daemon() {
                effects.extend(self.toggle_work_view(now));
            } else {
                effects.extend(self.work.reduce(WorkInput::Shown, now));
            }
        }
        if rebuild {
            self.rebuild_tree();
        }
        effects
    }

    /// Counts the full filtered Work tree, not just currently unfolded rows.
    #[must_use]
    pub fn work_counts(&self) -> (usize, usize) {
        (
            self.work_window_count,
            self.tree.iter().map(TreeNode::agent_count).sum(),
        )
    }

    /// Reporting roots in the full filtered tree, including collapsed threads.
    #[must_use]
    pub fn work_thread_count(&self) -> usize {
        self.tree
            .iter()
            .filter(|node| matches!(node, TreeNode::Team { .. }))
            .count()
    }

    fn restore_node(&mut self, selected: Option<&TreeNodeId>) {
        if let Some(path) = selected.and_then(|id| ancestor_ids(&self.tree, id)) {
            self.expanded.extend(path);
        }
        self.reflatten();
        if let Some(pos) =
            selected.and_then(|id| self.tree_rows.iter().position(|row| &row.node_id == id))
        {
            self.selected_index = pos;
        }
        self.clamp_selection();
    }

    /// Creates a new App with pick mode enabled (exit after jump).
    pub fn with_pick_mode() -> Self {
        let mut app = Self::new();
        app.pick_mode = true;
        app
    }

    /// Creates a new App that filters sessions to a specific tmux session.
    pub fn with_tmux_session_filter(session: String) -> Self {
        let mut app = Self::new();
        app.tmux_session_filter = Some(session);
        app
    }

    /// Updates the set of pane IDs belonging to the filtered tmux session.
    ///
    /// Triggers a tree rebuild if the pane set changed.
    pub fn update_filter_panes(&mut self, pane_ids: HashSet<String>) {
        if self.filter_pane_ids != pane_ids {
            self.filter_pane_ids = pane_ids;
            self.rebuild_tree();
        }
    }

    /// Rebuilds the tree only when the coordinator pane set changes.
    pub fn update_coordinator_panes(&mut self, panes: HashSet<String>) {
        if self.coordinator_panes != panes {
            self.coordinator_panes = panes;
            self.rebuild_tree();
            if let Some(pos) = self
                .last_selected_node
                .as_ref()
                .and_then(|id| self.tree_rows.iter().position(|row| &row.node_id == id))
            {
                self.selected_index = pos;
            }
        }
    }

    /// Classifies a row using its parent from the full, unfiltered session map.
    #[must_use]
    pub fn is_coordinator(&self, session: &SessionView) -> bool {
        crate::role::is_coordinator(session, self.parent_of(session), &self.coordinator_panes)
    }

    fn parent_of(&self, session: &SessionView) -> Option<&SessionView> {
        session
            .parent_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
    }

    /// Optimistically toggles an eligible row, returning the pane and desired
    /// value for the caller to write to tmux. Shared-parent-pane rows are no-ops.
    #[must_use]
    pub fn toggle_selected_coordinator(&mut self) -> Option<(String, bool)> {
        let session = self.selected_session()?;
        let pane = session.tmux_pane.clone()?;
        let parent = self.parent_of(session);
        let mut panes = self.coordinator_panes.clone();
        let on = panes.insert(pane.clone());
        if !crate::role::is_coordinator(session, parent, &panes) {
            return None;
        }
        if !on {
            panes.remove(&pane);
        }
        self.update_coordinator_panes(panes);
        Some((pane, on))
    }

    /// Updates the session list with new data from the daemon.
    ///
    /// Merges new sessions with existing ones (upsert behavior).
    /// This allows individual session updates without losing other sessions.
    pub fn update_sessions(&mut self, sessions: Vec<SessionView>) {
        for session in sessions {
            self.sessions.insert(session.id.clone(), session);
        }
        self.state = AppState::Connected;
        self.rebuild_tree();
    }

    /// Replaces all sessions with a new list from the daemon.
    ///
    /// Used for initial sync when connecting to the daemon.
    pub fn replace_sessions(&mut self, sessions: Vec<SessionView>) {
        self.sessions.clear();
        for session in sessions {
            self.sessions.insert(session.id.clone(), session);
        }
        self.state = AppState::Connected;
        self.rebuild_tree();
    }

    /// Removes a session from the session list by ID.
    ///
    /// If the removed session was selected or if selection is now out of bounds,
    /// the selection is clamped to a valid range.
    pub fn remove_session(&mut self, session_id: &str) {
        self.sessions.retain(|id, _| id.as_str() != session_id);
        self.rebuild_tree();
    }

    /// Rebuilds the tree from current session data and re-flattens it.
    ///
    /// Called after any session mutation. Preserves expand/collapse state.
    /// On first build (no expanded nodes yet), expands all nodes so the
    /// tree starts fully open.
    fn rebuild_tree(&mut self) {
        if let Some(row) = self.tree_rows.get(self.selected_index) {
            self.last_selected_node = Some(row.node_id.clone());
        }
        let mut sessions: Vec<SessionView> = if self.tmux_session_filter.is_some() {
            if self.filter_pane_ids.is_empty() {
                // Filter is set but pane IDs not loaded yet — show empty
                Vec::new()
            } else {
                self.sessions
                    .values()
                    .filter(|s| {
                        s.tmux_pane
                            .as_ref()
                            .is_some_and(|p| self.filter_pane_ids.contains(p))
                    })
                    .cloned()
                    .collect()
            }
        } else {
            self.sessions.values().cloned().collect()
        };
        let previous_matches = self.filter_matches.take();
        self.filter_matches = if self.filter_query.is_empty() {
            None
        } else {
            let refs: Vec<&SessionView> = sessions.iter().collect();
            let selection = crate::filter::select(&refs, &self.filter_query);
            sessions.retain(|s| selection.visible.contains(&s.id));
            Some(selection.matched)
        };
        let built = match self.view {
            AppView::Sessions => build_tree(&sessions),
            AppView::Work => {
                let windows = build_work_tree(&sessions, self.work.windows());
                self.work_window_count = windows
                    .iter()
                    .filter(|node| matches!(node, TreeNode::Window {id,..} if id != NO_WINDOW))
                    .count();
                match (self.work_grouping, self.work.org()) {
                    (WorkGrouping::Threads, Some(org)) => {
                        build_work_threads(&sessions, self.work.windows(), org)
                    }
                    _ => windows,
                }
            }
        };
        let previous_tree = std::mem::replace(&mut self.tree, built);
        crate::role::coordinators_first(&mut self.tree, &self.coordinator_panes);

        // On first build, expand everything so the tree starts open
        if self.view == AppView::Work {
            let previous_ids = all_node_ids(&previous_tree);
            self.expanded
                .extend(all_node_ids(&self.tree).difference(&previous_ids).cloned());
        } else if self.expanded.is_empty() && !self.tree.is_empty() {
            self.expanded = all_node_ids(&self.tree);
        }

        // Open the groups around sessions that just started matching or whose
        // groups changed (moved project, worktree level appeared), so a live
        // update never leaves a match hidden in a collapsed fold. Folds around
        // unchanged matches are left as the user set them.
        if let Some(matched) = &self.filter_matches {
            for id in matched {
                let node = TreeNodeId::Agent(id.clone());
                let Some(path) = ancestor_ids(&self.tree, &node) else {
                    continue;
                };
                let was_matched = previous_matches.as_ref().is_some_and(|p| p.contains(id));
                if !was_matched || ancestor_ids(&previous_tree, &node).as_ref() != Some(&path) {
                    self.expanded.extend(path);
                }
            }
        }

        if self.view == AppView::Work {
            let selected = self.last_selected_node.clone();
            self.restore_node(selected.as_ref());
        } else {
            self.reflatten();
            self.clamp_selection();
        }
    }

    /// Opens the `/` search prompt, keeping any existing query for editing.
    pub fn start_search(&mut self) {
        self.search_active = true;
    }

    /// Closes the search prompt and keeps the current filter applied.
    pub fn confirm_search(&mut self) {
        self.search_active = false;
    }

    /// Appends a character to the filter query.
    pub fn push_search_char(&mut self, c: char) {
        self.filter_query.push(c);
        self.apply_filter_change();
    }

    /// Removes the last character from the filter query.
    pub fn pop_search_char(&mut self) {
        if self.filter_query.pop().is_none() {
            return;
        }
        if self.filter_query.is_empty() {
            self.end_filter();
        } else {
            self.apply_filter_change();
        }
    }

    /// Closes the search prompt and removes the filter.
    pub fn clear_filter(&mut self) {
        self.search_active = false;
        self.filter_query.clear();
        self.end_filter();
    }

    /// Restores the fold state saved when the filter started, keeping the
    /// selected row and the groups around it open (like vim after a search).
    /// No-op when no filter is active.
    fn end_filter(&mut self) {
        // The query is global, so ending it also restores the hidden view's saved folds.
        self.inactive_filter_query.clear();
        if let Some(saved) = self.inactive_pre_filter_expanded.take() {
            self.inactive_expanded = saved;
        }
        let Some(saved) = self.pre_filter_expanded.take() else {
            return;
        };
        let selected = self
            .tree_rows
            .get(self.selected_index)
            .map(|r| r.node_id.clone())
            .or_else(|| self.last_selected_node.clone());
        self.rebuild_tree();
        // Open the selection's ancestors in the unfiltered tree; it can have
        // group levels (e.g. worktrees) that the filtered tree skipped.
        // Assigned after the rebuild, which expands everything when the set
        // is empty. With no saved state, open everything like a first build.
        let mut expanded = saved.unwrap_or_else(|| all_node_ids(&self.tree));
        if let Some(path) = selected
            .as_ref()
            .and_then(|id| ancestor_ids(&self.tree, id))
        {
            expanded.extend(path);
        }
        self.expanded = expanded;
        self.reflatten();
        if let Some(pos) =
            selected.and_then(|id| self.tree_rows.iter().position(|r| r.node_id == id))
        {
            self.selected_index = pos;
        }
        self.clamp_selection();
    }

    /// Returns true if the session is shown only as an ancestor of a match.
    pub fn is_filter_context(&self, id: &SessionId) -> bool {
        self.filter_matches
            .as_ref()
            .is_some_and(|matched| !matched.contains(id))
    }

    /// Rebuilds after a query edit, expanding every group so matches are
    /// never hidden in a collapsed fold, and selects the first match.
    fn apply_filter_change(&mut self) {
        if self.pre_filter_expanded.is_none() {
            self.pre_filter_expanded = Some((!self.tree.is_empty()).then(|| self.expanded.clone()));
        }
        self.rebuild_tree();
        self.expanded.extend(all_node_ids(&self.tree));
        self.reflatten();
        if let Some(matched) = &self.filter_matches {
            if let Some(pos) = self.tree_rows.iter().position(|r| {
                matches!(&r.kind, TreeRowKind::Agent { session } if matched.contains(&session.id))
            }) {
                self.selected_index = pos;
            }
        }
        self.clamp_selection();
    }

    /// Collapses every fold group in the tree (vim `zM`).
    ///
    /// Clears the expanded set so only top-level projects are visible,
    /// then moves the cursor to the top-level ancestor of the previous
    /// selection so the user stays oriented.
    pub fn collapse_all(&mut self) {
        let anchor = self.top_level_ancestor_id();
        self.expanded.clear();
        self.reflatten();
        if let Some(id) = anchor {
            if let Some(pos) = self.tree_rows.iter().position(|r| r.node_id == id) {
                self.selected_index = pos;
            }
        }
        self.clamp_selection();
    }

    /// Expands every fold group in the tree (vim `zR`).
    pub fn expand_all(&mut self) {
        self.expanded = all_node_ids(&self.tree);
        self.reflatten();
    }

    /// Opens the fold at the cursor (vim `zo`, also bound to `h` and `l`).
    ///
    /// If the selected row is a collapsed group, expands it. Otherwise
    /// (expanded group or leaf) this is a no-op. Never closes a fold.
    pub fn open_fold(&mut self) {
        if let Some(row) = self.tree_rows.get(self.selected_index) {
            if row.has_children && !self.expanded.contains(&row.node_id) {
                let id = row.node_id.clone();
                self.expanded.insert(id);
                self.reflatten();
            }
        }
    }

    /// Closes the fold at the cursor (vim `zc`).
    ///
    /// - Expanded group → collapses it.
    /// - Leaf (agent) → walks up to the nearest ancestor group and closes it
    ///   (matches vim's `zc` on a non-fold line).
    /// - Collapsed group → no-op (vim behavior — there's no fold to close
    ///   below the cursor).
    pub fn close_fold(&mut self) {
        let target = match self.tree_rows.get(self.selected_index) {
            Some(row) if row.has_children && self.expanded.contains(&row.node_id) => {
                Some(row.node_id.clone())
            }
            Some(row) if !row.has_children => self.nearest_ancestor_id(self.selected_index),
            _ => None,
        };
        if let Some(id) = target {
            self.set_node_expanded(&id, false);
        }
    }

    /// Toggles the fold at the cursor (vim `za`, also `Enter` on groups).
    ///
    /// On a group: flips its expand state. On a leaf: walks up to the
    /// nearest ancestor and toggles that — symmetric with `close_fold`.
    /// (Note: `Enter` only reaches this method for group rows — leaf
    /// rows are routed to the jump-to-session path.)
    pub fn toggle_fold(&mut self) {
        let target = match self.tree_rows.get(self.selected_index) {
            Some(row) if row.has_children => Some(row.node_id.clone()),
            Some(_) => self.nearest_ancestor_id(self.selected_index),
            None => None,
        };
        if let Some(id) = target {
            let want = !self.expanded.contains(&id);
            self.set_node_expanded(&id, want);
        }
    }

    // ------------------------------------------------------------------
    // Fold helpers
    // ------------------------------------------------------------------

    /// Sets a single node's expanded state, re-flattens, moves the cursor
    /// to that node, and clamps. Used by `close_fold` and `toggle_fold`
    /// (both of which may mutate selection via the walk-up path).
    fn set_node_expanded(&mut self, id: &TreeNodeId, expanded: bool) {
        if expanded {
            self.expanded.insert(id.clone());
        } else {
            self.expanded.remove(id);
        }
        self.reflatten();
        if let Some(pos) = self.tree_rows.iter().position(|r| &r.node_id == id) {
            self.selected_index = pos;
        }
        self.clamp_selection();
    }

    /// Re-flattens the tree from current expanded state.
    fn reflatten(&mut self) {
        self.tree_rows = flatten_tree(&self.tree, &self.expanded);
    }

    /// Returns the node_id of the nearest row strictly above `index` with
    /// smaller depth — the parent in the DFS-ordered tree.
    fn nearest_ancestor_id(&self, index: usize) -> Option<TreeNodeId> {
        let current_depth = self.tree_rows.get(index)?.depth;
        if current_depth == 0 {
            return None;
        }
        self.tree_rows
            .iter()
            .take(index)
            .rev()
            .find(|r| r.depth < current_depth)
            .map(|r| r.node_id.clone())
    }

    /// Returns the top-level (depth == 0) ancestor's node_id for the
    /// current selection.
    fn top_level_ancestor_id(&self) -> Option<TreeNodeId> {
        let row = self.tree_rows.get(self.selected_index)?;
        if row.depth == 0 {
            return Some(row.node_id.clone());
        }
        self.tree_rows
            .iter()
            .take(self.selected_index)
            .rev()
            .find(|r| r.depth == 0)
            .map(|r| r.node_id.clone())
    }

    /// Clamps the selected_index to a valid range based on current row count.
    fn clamp_selection(&mut self) {
        let row_count = self.tree_rows.len();
        if row_count == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= row_count {
            self.selected_index = row_count.saturating_sub(1);
        }
    }

    /// Marks the connection as disconnected and increments retry count.
    ///
    /// If already disconnected, increments the retry counter.
    /// If connected or connecting, transitions to disconnected with retry_count = 1.
    /// `reason` replaces the previous one: why the daemon rejected this
    /// connection, or `None`.
    pub fn mark_disconnected(&mut self, reason: Option<String>) {
        match &self.state {
            AppState::Disconnected {
                since, retry_count, ..
            } => {
                self.state = AppState::Disconnected {
                    since: *since,
                    retry_count: retry_count.saturating_add(1),
                    reason,
                };
            }
            AppState::Connected | AppState::Connecting => {
                self.state = AppState::Disconnected {
                    since: Utc::now(),
                    retry_count: 1,
                    reason,
                };
            }
        }
    }

    /// Returns sessions sorted by start time (newest first).
    ///
    /// Sessions are ordered by their `started_at` field in descending order,
    /// so the most recently started sessions appear first.
    #[cfg(test)]
    pub fn sessions_sorted(&self) -> Vec<&SessionView> {
        let mut sessions: Vec<&SessionView> = self.sessions.values().collect();
        // Sort by started_at descending (newest first)
        // Parse the ISO 8601 string for comparison
        sessions.sort_by(|a, b| {
            // Compare started_at strings in reverse order (newest first)
            // ISO 8601 strings sort correctly lexicographically
            b.started_at.cmp(&a.started_at)
        });
        sessions
    }

    /// Returns the currently selected session, if any.
    ///
    /// Returns `None` if no rows exist, the selection is out of bounds,
    /// or the selected row is not an Agent row.
    pub fn selected_session(&self) -> Option<&SessionView> {
        self.tree_rows.get(self.selected_index).and_then(|row| {
            if let TreeRowKind::Agent { ref session } = row.kind {
                Some(session)
            } else {
                None
            }
        })
    }

    /// Updates the captured output if it matches the currently tracked pane.
    pub fn update_capture(&mut self, pane_id: &str, lines: Vec<String>) {
        if self.capture_pane_id.as_deref() == Some(pane_id) {
            self.captured_output = lines;
        }
    }

    /// Directory whose `.beads/` the selected session's preview shows: the
    /// project root (where `.beads/` lives), else the worktree path.
    pub fn selected_beads_dir(&self) -> Option<&str> {
        let session = self.selected_session()?;
        session
            .project_root
            .as_deref()
            .or(session.worktree_path.as_deref())
    }

    /// Cached in-progress Beads task for the selected session's directory.
    pub fn selected_beads_task(&self) -> Option<&BeadsTask> {
        self.beads_tasks.get(self.selected_beads_dir()?)?.as_ref()
    }

    /// Records the result of a completed Beads read for `dir`.
    pub fn update_beads_task(&mut self, dir: String, task: Option<BeadsTask>) {
        self.beads_tasks.insert(dir, task);
    }

    /// Navigates to the next row (downward), wrapping around if needed.
    #[cfg(test)]
    pub fn select_next(&mut self) {
        let row_count = self.tree_rows.len();
        if row_count == 0 {
            self.selected_index = 0;
            return;
        }
        self.selected_index = (self.selected_index.saturating_add(1)) % row_count;
    }

    /// Navigates to the previous row (upward), wrapping around if needed.
    #[cfg(test)]
    pub fn select_previous(&mut self) {
        let row_count = self.tree_rows.len();
        if row_count == 0 {
            self.selected_index = 0;
            return;
        }
        if self.selected_index == 0 {
            self.selected_index = row_count.saturating_sub(1);
        } else {
            self.selected_index = self.selected_index.saturating_sub(1);
        }
    }

    /// Moves selection down by `n`, clamping at the last row.
    pub fn select_down(&mut self, n: usize) {
        let row_count = self.tree_rows.len();
        if row_count == 0 {
            self.selected_index = 0;
            return;
        }
        self.selected_index = self
            .selected_index
            .saturating_add(n)
            .min(row_count.saturating_sub(1));
    }

    /// Moves selection up by `n`, clamping at the first session.
    pub fn select_up(&mut self, n: usize) {
        self.selected_index = self.selected_index.saturating_sub(n);
    }

    /// Jumps to absolute index, clamped to `[0, len-1]`.
    pub fn select_go_to(&mut self, index: usize) {
        let row_count = self.tree_rows.len();
        if row_count == 0 {
            self.selected_index = 0;
            return;
        }
        self.selected_index = index.min(row_count.saturating_sub(1));
    }

    /// Moves down by `n * (viewport_height / 2)`, clamping at last session.
    pub fn select_half_page_down(&mut self, n: usize, viewport_height: u16) {
        let distance = n.saturating_mul((viewport_height as usize) / 2);
        self.select_down(distance);
    }

    /// Moves up by `n * (viewport_height / 2)`, clamping at first session.
    pub fn select_half_page_up(&mut self, n: usize, viewport_height: u16) {
        let distance = n.saturating_mul((viewport_height as usize) / 2);
        self.select_up(distance);
    }

    /// Whether this named-session leaf, not a pane-sharing child, waits for the owner.
    #[must_use]
    pub fn has_waiting_decision(&self, session: &SessionView) -> bool {
        session.parent_session_id.is_none()
            && session
                .tmux_pane
                .as_ref()
                .is_some_and(|pane| self.decision_panes.contains(pane))
    }

    /// Moves to the next (`forward`) or previous agent needing input or an owner
    /// decision, wrapping in tree order. Opens collapsed ancestors; respects filters.
    pub fn select_attention(&mut self, forward: bool) {
        let all_rows = flatten_tree(&self.tree, &all_node_ids(&self.tree));
        let current = self
            .tree_rows
            .get(self.selected_index)
            .and_then(|row| all_rows.iter().position(|r| r.node_id == row.node_id))
            .unwrap_or(0);
        let len = all_rows.len();
        let target = (1..=len)
            .map(|step| {
                if forward {
                    (current + step) % len
                } else {
                    (current + len - step) % len
                }
            })
            .filter_map(|i| all_rows.get(i))
            .find(|r| matches!(&r.kind, TreeRowKind::Agent { session } if session.needs_attention || self.has_waiting_decision(session)))
            .map(|r| r.node_id.clone());
        let Some(target) = target else {
            return;
        };
        if let Some(path) = ancestor_ids(&self.tree, &target) {
            self.expanded.extend(path);
        }
        self.reflatten();
        if let Some(pos) = self.tree_rows.iter().position(|r| r.node_id == target) {
            self.selected_index = pos;
        }
    }

    /// Advances the blink animation by one tick.
    ///
    /// Should be called every 100ms (on each event loop tick).
    /// Toggles `blink_visible` every 5 ticks (500ms).
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        // Toggle blink every 5 ticks (500ms at 100ms tick rate)
        if self.tick_count.is_multiple_of(5) {
            self.blink_visible = !self.blink_visible;
        }
    }

    /// Sets the quit flag to true, signaling the application should exit.
    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Toggles the help popup visibility.
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
        self.help_scroll = 0;
    }

    /// Returns the number of sessions currently tracked.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Returns the total cost across all sessions in USD.
    pub fn total_cost(&self) -> f64 {
        self.sessions.values().map(|s| s.cost_usd).sum()
    }

    /// Returns the average context usage percentage across all sessions.
    ///
    /// Returns 0.0 if no sessions exist.
    pub fn average_context(&self) -> f64 {
        if self.sessions.is_empty() {
            return 0.0;
        }
        let total: f64 = self.sessions.values().map(|s| s.context_percentage).sum();
        total / self.sessions.len() as f64
    }

    /// Returns the number of sessions that need attention (waiting for input).
    pub fn attention_count(&self) -> usize {
        self.sessions.values().filter(|s| s.needs_attention).count()
    }

    /// Returns the number of sessions currently working (actively processing).
    pub fn working_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|s| matches!(s.status, atm_core::SessionStatus::Working))
            .count()
    }
}

/// Returns the IDs of the groups enclosing `target`, outermost first, or
/// `None` if `target` is not in the tree.
fn ancestor_ids(tree: &[TreeNode], target: &TreeNodeId) -> Option<Vec<TreeNodeId>> {
    tree.iter().find_map(|node| {
        if &node.node_id() == target {
            return Some(Vec::new());
        }
        let children = match node {
            TreeNode::Project { children, .. }
            | TreeNode::Worktree { children, .. }
            | TreeNode::Team { children, .. }
            | TreeNode::Window { children, .. } => children,
            TreeNode::Agent { subagents, .. } => subagents,
        };
        let mut path = ancestor_ids(children, target)?;
        path.insert(0, node.node_id());
        Some(path)
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn work_capabilities(windows: bool) -> WorkInput {
        let mut methods = vec!["org.tree".into()];
        if windows {
            methods.push("session.windows".into());
        }
        WorkInput::Capabilities {
            methods,
            watch: false,
        }
    }

    // Break caught: a constructor or compact/sidebar path keeps the old start tab.
    #[test]
    fn default_work_in_full_compact_pick_and_filtered_apps() {
        assert_eq!(AppView::default(), AppView::Work);
        for mut app in [
            App::new(),
            App::default(),
            App::with_pick_mode(),
            App::with_tmux_session_filter("work".into()),
        ] {
            for compact in [false, true] {
                app.compact = compact;
                assert_eq!(app.view, AppView::Work);
                assert!(app.reduce_work(WorkInput::Wake, Duration::ZERO).is_empty());
            }
        }
    }

    // Break caught: an older daemon gets an initial Work error, or reconnect steals the tab again.
    #[test]
    fn first_unsupported_capabilities_select_sessions_once() {
        let mut app = App::new();
        app.view = AppView::Work;
        let effects = app.reduce_work(work_capabilities(false), Duration::ZERO);
        assert_eq!(app.view, AppView::Sessions);
        assert!(effects.is_empty());
        let _ = app.toggle_work_view(Duration::ZERO);
        assert_eq!(app.view, AppView::Work);
        for windows in [false, true, false] {
            let _ = app.reduce_work(work_capabilities(windows), Duration::from_secs(1));
            assert_eq!(app.view, AppView::Work);
        }
        assert!(app.work.needs_newer_daemon());
    }

    // Break caught: default Work never receives Shown, or starts reads before capabilities.
    #[test]
    fn modern_capabilities_start_work_without_w() {
        let mut app = App::new();
        assert!(app.reduce_work(WorkInput::Wake, Duration::ZERO).is_empty());
        let effects = app.reduce_work(work_capabilities(true), Duration::ZERO);
        assert_eq!(app.view, AppView::Work);
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, WorkEffect::RequestWindows(_)))
                .count(),
            1
        );
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, WorkEffect::RequestOrg(_)))
                .count(),
            1
        );
        assert!(effects.contains(&WorkEffect::WakeAt(Duration::from_secs(3))));
        let effects = app.reduce_work(work_capabilities(true), Duration::from_secs(1));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, WorkEffect::RequestWindows(_) | WorkEffect::RequestOrg(_))));
    }

    // Break caught: the first supported connection leaves a later reconnect free to steal Work.
    #[test]
    fn first_supported_capabilities_do_not_enable_a_later_fallback() {
        let mut app = App::new();
        let _ = app.reduce_work(work_capabilities(true), Duration::ZERO);
        let effects = app.reduce_work(work_capabilities(false), Duration::from_secs(1));
        assert_eq!(app.view, AppView::Work);
        assert!(app.work.needs_newer_daemon());
        assert!(effects.is_empty());
    }

    // Break caught: automatic fallback overrides a user's deliberate pre-handshake choice.
    #[test]
    fn explicit_tab_before_capabilities_wins() {
        for toggles in [1, 2] {
            let mut app = App::new();
            app.view = AppView::Work;
            for _ in 0..toggles {
                let _ = app.toggle_work_view(Duration::ZERO);
            }
            let selected = app.view;
            let _ = app.reduce_work(work_capabilities(false), Duration::ZERO);
            assert_eq!(app.view, selected);
        }
    }

    // Break caught: selecting the current tab destroys folds/selection or disarms the next poll.
    #[test]
    fn same_numbered_tab_keeps_selection_folds_and_next_wake() {
        let mut app = work_app();
        let effects = app.toggle_work_view(Duration::ZERO);
        work_reply(&mut app, effects, false);
        let selected = app.selected_session().unwrap().id.clone();
        app.collapse_all();
        let folds = app.expanded.clone();
        let effects = app.select_tab(1, Duration::ZERO).unwrap();
        assert_eq!(effects, [WorkEffect::WakeAt(Duration::from_secs(3))]);
        assert_eq!(app.expanded, folds);
        assert_eq!(app.selected_session().map(|s| &s.id), None);
        assert!(app.select_tab(3, Duration::from_secs(1)).is_none());
        assert_eq!(app.work_now, Duration::ZERO);
        assert_eq!(app.reduce_work(WorkInput::Wake, Duration::ZERO), effects);
        app.expand_all();
        select(&mut app, selected.as_str());
        let effects = app.select_tab(1, Duration::ZERO).unwrap();
        assert_eq!(effects, [WorkEffect::WakeAt(Duration::from_secs(3))]);
        assert_eq!(app.selected_session().unwrap().id, selected);
    }

    // Break caught: an invalid tab consumes fallback, or valid same-tab intent is forgotten.
    #[test]
    fn numbered_tab_choice_controls_initial_fallback_only_when_valid() {
        for (number, expected) in [(1, AppView::Work), (3, AppView::Sessions)] {
            let mut app = App::new();
            let _ = app.select_tab(number, Duration::ZERO);
            let _ = app.reduce_work(work_capabilities(false), Duration::ZERO);
            assert_eq!(app.view, expected);
        }
    }

    // Synthetic: cross-repo reporting graph with nested leads, not captured owner data.
    fn thread_org_value() -> serde_json::Value {
        serde_json::json!({"seq":42,"projects":[
            {"seats":[
                {"agent_id":"alpha/chief","pane":"%1","manager":"owner","tasks":[]},
                {"agent_id":"alpha/lead","pane":"%2","manager":"alpha/chief","tasks":["t1"]},
                {"agent_id":"alpha/worker","pane":"%3","manager":"alpha/lead","tasks":["t2"]}],
             "tasks":[{"task_id":"t1","title":"Improve cache behavior"},{"task_id":"t2","title":"Fix reconnect watch"}]},
            {"seats":[{"agent_id":"beta/worker","pane":"%4","manager":"alpha/lead","tasks":[]}],"tasks":[]}
        ],"needs_owner":{"items":[]}})
    }

    fn thread_org() -> crate::work::WorkOrg {
        serde_json::from_value(thread_org_value()).unwrap()
    }

    fn thread_windows() -> crate::work::WorkWindows {
        serde_json::from_value(serde_json::json!({"state":"ok","windows":[
            {"id":"@1","name":"primary","links":[{"session":"work","index":0}],"panes":["%1","%2","%3"]},
            {"id":"@2","name":"secondary","links":[{"session":"work","index":1}],"panes":["%4"]}
        ]})).unwrap()
    }

    fn thread_app(org: crate::work::WorkOrg) -> App {
        let mut app = App::new();
        app.update_sessions(
            (1..=4)
                .map(|n| SessionView {
                    id: format!("s{n}").into(),
                    id_short: format!("s{n}"),
                    tmux_pane: Some(format!("%{n}")),
                    project_root: Some("/fixtures/repo".into()),
                    ..Default::default()
                })
                .collect(),
        );
        let effects = app.reduce_work(work_capabilities(true), Duration::ZERO);
        thread_load(&mut app, effects, org);
        app
    }

    fn thread_load(app: &mut App, effects: Vec<WorkEffect>, org: crate::work::WorkOrg) {
        for effect in effects {
            let input = match effect {
                WorkEffect::RequestWindows(generation) => WorkInput::WindowsReply {
                    generation,
                    result: Ok(thread_windows()),
                },
                WorkEffect::RequestOrg(generation) => WorkInput::OrgReply {
                    generation,
                    result: Ok(org.clone()),
                },
                _ => continue,
            };
            let _ = app.reduce_work(input, Duration::ZERO);
        }
    }

    fn thread_members(app: &App) -> Vec<(String, Vec<String>)> {
        let mut groups = Vec::<(String, Vec<String>)>::new();
        for row in &app.tree_rows {
            match &row.kind {
                TreeRowKind::Team { name } => groups.push((name.clone(), vec![])),
                TreeRowKind::Window { id, .. } => groups.push((id.clone(), vec![])),
                TreeRowKind::Agent { session } => {
                    groups.last_mut().unwrap().1.push(session.id.to_string())
                }
                _ => panic!("unexpected Work group"),
            }
        }
        groups
    }

    // Break caught: org replies never regroup Work, cross-repo members are split, or windows are fake threads.
    #[test]
    fn threads_use_existing_cross_repo_reporting_lines() {
        let app = thread_app(thread_org());
        let groups = thread_members(&app);
        assert_eq!(
            groups.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["alpha/chief", "alpha/lead"]
        );
        let lead = groups.iter().find(|(id, _)| id == "alpha/lead").unwrap();
        assert_eq!(
            lead.1
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["s2", "s3", "s4"].into_iter().collect()
        );
        assert_eq!(app.work_counts(), (2, 4));
        assert_eq!(
            app.sessions[&SessionId::from("s4")].tmux_pane.as_deref(),
            Some("%4")
        );
    }

    // Break caught: physical-window order replaces newest-first ordering inside a reporting thread.
    #[test]
    fn thread_rows_keep_newest_and_coordinator_order_across_windows() {
        let mut app = thread_app(thread_org());
        let rows = (1..=4)
            .map(|n| {
                let mut row = app.sessions[&SessionId::from(format!("s{n}"))].clone();
                row.started_at = format!("2026-01-15T12:00:0{n}Z");
                row
            })
            .collect();
        app.update_sessions(rows);
        let members = |app: &App| {
            thread_members(app)
                .into_iter()
                .find(|(id, _)| id == "alpha/lead")
                .unwrap()
                .1
        };
        assert_eq!(members(&app), ["s4", "s3", "s2"]);
        app.update_coordinator_panes(["%2".into()].into_iter().collect());
        assert_eq!(members(&app), ["s2", "s4", "s3"]);
    }

    // Break caught: a managing seat is emitted in both its parent's and its own team.
    #[test]
    fn nested_leads_are_not_duplicated_in_chief_thread() {
        let mut app = thread_app(thread_org());
        assert_eq!(thread_members(&app).first().unwrap().1, ["s1"]);
        assert_eq!(agent_ids(&app).len(), 4);
        select(&mut app, "s3");
        app.close_fold();
        assert_eq!(thread_members(&app).last().unwrap().0, "alpha/lead");
        assert!(!app
            .expanded
            .contains(&TreeNodeId::Team("alpha/lead".into())));
    }

    // Break caught: short names, unknown managers, invalid chains or ambiguous pane claims merge seats.
    #[test]
    fn same_short_names_and_missing_or_broken_lines_do_not_merge() {
        let mut org = thread_org_value();
        org["projects"][1]["seats"] = serde_json::json!([
            {"agent_id":"beta/lead","pane":"%4","manager":"owner","tasks":[]},
            {"agent_id":"beta/worker","pane":"%5","manager":"beta/lead","tasks":[]},
            {"agent_id":"beta/missing","pane":"%6","manager":"absent/lead","tasks":[]},
            {"agent_id":"beta/broken","pane":"%7","manager":"alpha/lead","line_error":"broken","tasks":[]},
            {"agent_id":"beta/cycle-a","pane":"%8","manager":"beta/cycle-b","tasks":[]},
            {"agent_id":"beta/cycle-b","pane":"%9","manager":"beta/cycle-a","tasks":[]},
            {"agent_id":"beta/duplicate","pane":"%3","manager":"alpha/lead","tasks":[]},
            {"agent_id":"alpha/paneless","pane":null,"manager":"alpha/lead","tasks":[]}
        ]);
        let mut app = thread_app(serde_json::from_value(org).unwrap());
        app.update_sessions(
            (5..=9)
                .map(|n| SessionView {
                    id: format!("s{n}").into(),
                    tmux_pane: Some(format!("%{n}")),
                    ..Default::default()
                })
                .collect(),
        );
        let groups = thread_members(&app);
        assert!(groups.iter().any(|(id, _)| id == "alpha/lead"));
        assert!(groups.iter().any(|(id, ids)| id == "beta/lead"
            && ids.contains(&"s5".into())
            && !ids.contains(&"s3".into())));
        let fallback: Vec<_> = groups
            .iter()
            .filter(|(id, _)| id.starts_with('@') || id == NO_WINDOW)
            .flat_map(|(_, ids)| ids)
            .map(String::as_str)
            .collect();
        for id in ["s3", "s6", "s7", "s8", "s9"] {
            assert!(fallback.contains(&id), "{groups:?}");
        }
        assert_eq!(agent_ids(&app).len(), 9);
    }

    // Break caught: task/window/filter changes rename Team identity or steal the selected pane.
    #[test]
    fn thread_identity_survives_title_window_and_filter_changes() {
        let mut app = thread_app(thread_org());
        select(&mut app, "s3");
        let id = TreeNodeId::Team("alpha/lead".into());
        assert!(app.expanded.contains(&id));
        let mut org = thread_org();
        org.projects
            .first_mut()
            .unwrap()
            .tasks
            .first_mut()
            .unwrap()
            .title = "Different work".into();
        let effects = app.reduce_work(WorkInput::Refresh, Duration::ZERO);
        thread_load(&mut app, effects, org);
        type_query(&mut app, "s3");
        assert!(app.tree_rows.iter().any(|row| row.node_id == id));
        assert_eq!(app.selected_session().unwrap().id.as_str(), "s3");
        let effects = app.reduce_work(WorkInput::Refresh, Duration::ZERO);
        for effect in effects {
            let input = match effect {
                WorkEffect::RequestWindows(generation) => {
                    let mut windows = thread_windows();
                    windows
                        .windows
                        .first_mut()
                        .unwrap()
                        .panes
                        .retain(|p| p.0 != "%3");
                    windows
                        .windows
                        .last_mut()
                        .unwrap()
                        .panes
                        .push(crate::work::PaneId("%3".into()));
                    WorkInput::WindowsReply {
                        generation,
                        result: Ok(windows),
                    }
                }
                WorkEffect::RequestOrg(generation) => WorkInput::OrgReply {
                    generation,
                    result: Ok(thread_org()),
                },
                _ => continue,
            };
            let _ = app.reduce_work(input, Duration::ZERO);
        }
        assert!(app.tree_rows.iter().any(|row| row.node_id == id));
        assert_eq!(app.work_counts(), (1, 1));
        assert_eq!(
            app.selected_session().unwrap().tmux_pane.as_deref(),
            Some("%3")
        );
        let _ = app.reduce_work(work_capabilities(false), Duration::ZERO);
        let _ = app.toggle_work_view(Duration::ZERO);
        let rows = format!("{:?}", app.tree_rows);
        let effects = app.reduce_work(WorkInput::Refresh, Duration::ZERO);
        assert!(effects.is_empty());
        assert_eq!(format!("{:?}", app.tree_rows), rows);
    }

    // Break caught: absent lead sessions erase visible reporters, or org-only seats invent rows.
    #[test]
    fn paneless_lead_heads_visible_reporters_without_inventing_sessions() {
        let mut org = thread_org();
        org.projects
            .first_mut()
            .unwrap()
            .seats
            .get_mut(1)
            .unwrap()
            .pane = None;
        let mut app = thread_app(org);
        app.remove_session("s2");
        let lead = thread_members(&app)
            .into_iter()
            .find(|(id, _)| id == "alpha/lead")
            .unwrap();
        assert_eq!(
            lead.1
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            ["s3".to_owned(), "s4".to_owned()].into_iter().collect()
        );
        assert_eq!(agent_ids(&app).len(), 3);
    }

    // Break caught: a terminal org reply for hidden Work regroups the active Sessions tree.
    #[test]
    fn hidden_thread_reply_releases_its_slot_without_rebuilding_sessions() {
        let mut app = App::new();
        app.update_sessions(vec![SessionView {
            id: "s1".into(),
            tmux_pane: Some("%1".into()),
            ..Default::default()
        }]);
        let effects = app.reduce_work(work_capabilities(true), Duration::ZERO);
        let _ = app.toggle_work_view(Duration::ZERO);
        let rows = list_buffer(&app);
        thread_load(&mut app, effects, thread_org());
        assert_eq!(app.view, AppView::Sessions);
        assert_eq!(list_buffer(&app), rows);
        let effects = app.toggle_work_view(Duration::ZERO);
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, WorkEffect::RequestOrg(_)))
                .count(),
            1
        );
    }

    fn sessions_app() -> App {
        let mut app = App::new();
        app.view = AppView::Sessions;
        app
    }

    fn work_app() -> App {
        let mut app = sessions_app();
        app.work_grouping = WorkGrouping::Windows;
        let mut parent = role_session("parent", Some("%1"), None, "2");
        parent.project_root = Some("/fixtures/alpha".into());
        parent.worktree_path = parent.project_root.clone();
        parent.worktree_branch = Some("main".into());
        let child = SessionView {
            id: "child".into(),
            id_short: "child".into(),
            parent_session_id: Some(parent.id.clone()),
            ..parent.clone()
        };
        let mut other = role_session("other", Some("%2"), None, "1");
        other.project_root = Some("/fixtures/beta".into());
        other.worktree_path = other.project_root.clone();
        app.update_sessions(vec![parent, child, other]);
        select(&mut app, "parent");
        app
    }
    fn work_windows(moved: bool) -> crate::work::WorkWindows {
        serde_json::from_value(serde_json::json!({"state":"ok","windows":[
            {"id":"@1","name":"alpha","links":[{"session":"work","index":0}],"panes": if moved {vec!["%2"]} else {vec!["%1"]}},
            {"id":"@2","name":"beta","links":[{"session":"work","index":1}],"panes": if moved {vec!["%1"]} else {vec!["%2"]}}
        ]})).unwrap()
    }
    fn work_reply(app: &mut App, effects: Vec<crate::work::WorkEffect>, moved: bool) {
        let generation = effects
            .into_iter()
            .find_map(|effect| match effect {
                crate::work::WorkEffect::RequestWindows(generation) => Some(generation),
                _ => None,
            })
            .unwrap();
        let _ = app.reduce_work(
            crate::work::WorkInput::WindowsReply {
                generation,
                result: Ok(work_windows(moved)),
            },
            std::time::Duration::ZERO,
        );
    }
    fn list_buffer(app: &App) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| crate::ui::session_list::render_session_list(frame, frame.area(), app))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    // Break caught: toggling changes SessionView identity, Sessions order/folds, or its rendered buffer.
    #[test]
    fn work_toggle_twice_restores_sessions_rows() {
        use crate::keybinding::{InputHandler, UiAction};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = work_app();
        let rows = format!("{:?}", app.tree_rows);
        let buffer = list_buffer(&app);
        let folds = app.expanded.clone();
        let mut handler = InputHandler::new();
        for i in 0..2 {
            assert_eq!(
                handler.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
                Some(UiAction::ToggleWorkView)
            );
            let effects = app.toggle_work_view(std::time::Duration::ZERO);
            if i == 0 {
                work_reply(&mut app, effects, false);
                assert_eq!(app.view, AppView::Work);
                assert_eq!(app.work_counts(), (2, 3));
                let cloned = app.clone();
                assert_eq!(cloned.work.windows().len(), 2);
            }
            assert_eq!(app.selected_session().unwrap().id.as_str(), "parent");
        }
        assert_eq!(app.view, AppView::Sessions);
        assert_eq!(format!("{:?}", app.tree_rows), rows);
        assert_eq!(app.expanded, folds);
        assert_eq!(list_buffer(&app), buffer);
    }

    // Break caught: an intentionally empty zM set is confused with an uninitialized view.
    #[test]
    fn work_folds_are_per_view_including_empty_sets() {
        let mut app = work_app();
        app.collapse_all();
        let sessions = format!("{:?}", app.tree_rows);
        let effects = app.toggle_work_view(std::time::Duration::ZERO);
        work_reply(&mut app, effects, false);
        app.collapse_all();
        assert!(app.expanded.is_empty());
        let _ = app.toggle_work_view(std::time::Duration::ZERO);
        assert_eq!(format!("{:?}", app.tree_rows), sessions);
        let effects = app.toggle_work_view(std::time::Duration::ZERO);
        work_reply(&mut app, effects, false);
        assert!(app.expanded.is_empty());
        assert_eq!(app.tree_rows.len(), 2);
        // A periodic successful reply must not silently unfold Work's zM.
        let effects = app.reduce_work(crate::work::WorkInput::Refresh, std::time::Duration::ZERO);
        work_reply(&mut app, effects, false);
        assert!(app.expanded.is_empty());
        assert_eq!(app.tree_rows.len(), 2);
    }

    // Break caught: ending a global filter in one view loses the other's pre-filter folds.
    #[test]
    fn work_folds_keep_per_view_filter_snapshots() {
        let mut app = work_app();
        let effects = app.toggle_work_view(std::time::Duration::ZERO);
        work_reply(&mut app, effects, false);
        app.collapse_all();
        let _ = app.toggle_work_view(std::time::Duration::ZERO);
        app.collapse_all();
        app.start_search();
        for c in "parent".chars() {
            app.push_search_char(c);
        }
        assert_eq!(app.selected_session().unwrap().id.as_str(), "parent");
        let _ = app.toggle_work_view(std::time::Duration::ZERO);
        assert_eq!(app.selected_session().unwrap().id.as_str(), "parent");
        app.clear_filter();
        assert!(!app.expanded.contains(&TreeNodeId::Window("@2".into())));
        let _ = app.toggle_work_view(std::time::Duration::ZERO);
        assert_eq!(app.selected_session().unwrap().id.as_str(), "parent");
        assert!(app
            .tree_rows
            .iter()
            .any(|r| matches!(&r.kind, TreeRowKind::Project {name,..} if name == "beta")));
        assert!(!app
            .expanded
            .contains(&TreeNodeId::Project("/fixtures/beta".into())));
    }

    // Break caught: sorting/regrouping leaves the cursor at an unrelated numeric row.
    #[test]
    fn work_regrouping_keeps_agent_selection() {
        let mut app = work_app();
        let effects = app.toggle_work_view(std::time::Duration::ZERO);
        work_reply(&mut app, effects, false);
        let effects = app.reduce_work(crate::work::WorkInput::Refresh, std::time::Duration::ZERO);
        work_reply(&mut app, effects, true);
        assert_eq!(app.selected_session().unwrap().id.as_str(), "parent");
        assert!(app.expanded.contains(&TreeNodeId::Window("@2".into())));
        app.remove_session("parent");
        assert!(app.selected_index < app.tree_rows.len());
        assert_eq!(
            role_row_ids(&app)
                .iter()
                .filter(|id| **id == "child")
                .count(),
            1
        );
    }

    // Break caught: a hidden reply rebuilds Sessions (including a still-active filter/cursor).
    #[test]
    fn work_hidden_replies_do_not_rebuild_sessions() {
        let mut app = work_app();
        let effects = app.toggle_work_view(std::time::Duration::ZERO);
        let _ = app.toggle_work_view(std::time::Duration::ZERO);
        app.start_search();
        app.push_search_char('p');
        let rows = format!("{:?}", app.tree_rows);
        let folds = app.expanded.clone();
        work_reply(&mut app, effects, false);
        assert_eq!(format!("{:?}", app.tree_rows), rows);
        assert_eq!(app.expanded, folds);
        assert!(app.work.windows().is_empty());
    }

    // Break caught: Work invents a second row/action model or folded counts omit subagents.
    #[test]
    fn work_window_enter_folds_and_agents_keep_action_identity() {
        let mut app = work_app();
        let effects = app.toggle_work_view(std::time::Duration::ZERO);
        work_reply(&mut app, effects, false);
        app.select_go_to(0);
        assert!(app.selected_session().is_none());
        app.toggle_fold();
        assert!(!app.tree_rows.first().unwrap().is_expanded);
        assert_eq!(app.tree_rows.first().unwrap().agent_count, 2);
        assert_eq!(app.work_counts(), (2, 3));
        app.open_fold();
        select(&mut app, "child");
        assert_eq!(
            app.selected_session().unwrap().tmux_pane.as_deref(),
            Some("%1")
        );
        assert_eq!(
            app.selected_session()
                .unwrap()
                .parent_session_id
                .as_ref()
                .unwrap()
                .as_str(),
            "parent"
        );
        let _ = app.toggle_work_view(std::time::Duration::ZERO);
        assert_eq!(app.selected_session().unwrap().id.as_str(), "child");
    }

    // Break caught: a query edited in Work leaves its new matches hidden behind Sessions' old folds.
    #[test]
    fn work_filter_edits_open_new_matches_in_the_other_view_only_when_changed() {
        let mut app = work_app();
        let arriving = app.sessions.get(&SessionId::new("other")).unwrap().clone();
        app.remove_session("other");
        app.collapse_all();
        app.start_search();
        for c in "/fixtures/a".chars() {
            app.push_search_char(c);
        }
        app.confirm_search();
        let effects = app.toggle_work_view(Duration::ZERO);
        work_reply(&mut app, effects, false);
        // A new project arrives while Sessions is hidden and the old query excludes it.
        app.update_sessions(vec![arriving]);
        app.start_search();
        app.pop_search_char();
        app.push_search_char('b');
        app.confirm_search();
        assert_eq!(app.selected_session().unwrap().id.as_str(), "other");
        app.close_fold();
        assert!(app.selected_session().is_none());
        let _ = app.toggle_work_view(Duration::ZERO);
        assert_eq!(
            role_row_ids(&app),
            ["other"],
            "the current filter's new match must be visible"
        );
        app.collapse_all();
        let _ = app.toggle_work_view(Duration::ZERO);
        let _ = app.toggle_work_view(Duration::ZERO);
        assert!(
            role_row_ids(&app).is_empty(),
            "an unchanged query must preserve deliberate folds"
        );
        let _ = app.toggle_work_view(Duration::ZERO);
        app.clear_filter();
        app.start_search();
        for c in "/fixtures/b".chars() {
            app.push_search_char(c);
        }
        app.confirm_search();
        app.close_fold();
        let _ = app.toggle_work_view(Duration::ZERO);
        assert_eq!(
            role_row_ids(&app),
            ["other"],
            "a restarted query must also reveal its matches"
        );
    }

    fn create_test_session(id: &str, started_at: &str) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.get(..8).unwrap_or(id).to_string(),
            agent_type: "general".to_string(),
            model: "Opus 4.5".to_string(),
            status: atm_core::SessionStatus::Working,
            status_label: "working".to_string(),
            activity_detail: None,
            should_blink: false,
            status_icon: ">".to_string(),
            context_percentage: 25.0,
            context_display: "25%".to_string(),
            context_warning: false,
            context_critical: false,
            cost_display: "$0.50".to_string(),
            cost_usd: 0.50,
            duration_display: "5m".to_string(),
            duration_seconds: 300.0,
            lines_display: "+100 -20".to_string(),
            working_directory: Some("/home/user/project".to_string()),

            needs_attention: false,
            last_activity_display: "10s ago".to_string(),
            age_display: "5m ago".to_string(),
            started_at: started_at.to_string(),
            last_activity: "2024-01-15T10:05:00Z".to_string(),
            tmux_pane: None,
            ..Default::default()
        }
    }

    fn role_session(id: &str, pane: Option<&str>, parent: Option<&str>, time: &str) -> SessionView {
        let mut session = create_test_session(id, time);
        session.tmux_pane = pane.map(str::to_string);
        session.parent_session_id = parent.map(SessionId::new);
        session
    }

    fn role_row_ids(app: &App) -> Vec<&str> {
        app.tree_rows
            .iter()
            .filter_map(|row| match &row.kind {
                TreeRowKind::Agent { session } => Some(session.id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn coordinator_updates_reorder_without_losing_selection_or_rebuilding_unchanged_sets() {
        let mut app = sessions_app();
        assert!(app.coordinator_panes.is_empty());
        app.update_sessions(vec![
            role_session("old", Some("%1"), None, "1"),
            role_session("new", Some("%2"), None, "2"),
        ]);
        select(&mut app, "old");
        app.update_coordinator_panes(HashSet::from(["%1".into()]));
        assert_eq!(role_row_ids(&app), ["old", "new"]);
        assert_eq!(app.selected_session().unwrap().id.as_str(), "old");
        app.update_coordinator_panes(HashSet::new());
        assert_eq!(role_row_ids(&app), ["new", "old"]);
        assert_eq!(app.selected_session().unwrap().id.as_str(), "old");
        // A rebuild would replace these cached rows even though roles did not change.
        app.tree_rows.clear();
        app.update_coordinator_panes(HashSet::new());
        assert!(app.tree_rows.is_empty());
    }

    #[test]
    fn coordinator_classification_and_toggle_use_full_map_even_when_parent_rows_are_hidden() {
        let mut app = sessions_app();
        app.update_sessions(vec![
            role_session("lead", Some("%1"), None, "1"),
            role_session("child", Some("%1"), Some("lead"), "2"),
            role_session("mate", Some("%2"), Some("lead"), "3"),
            role_session("grandchild", Some("%2"), Some("mate"), "4"),
            role_session("orphan", Some("%3"), Some("missing"), "5"),
            role_session("no-pane", None, None, "6"),
        ]);
        app.update_coordinator_panes(HashSet::from(["%1".into(), "%2".into(), "%3".into()]));
        for (id, expected) in [
            ("lead", true),
            ("child", false),
            ("mate", true),
            ("grandchild", false),
            ("orphan", true),
            ("no-pane", false),
        ] {
            assert_eq!(
                app.is_coordinator(app.sessions.get(&SessionId::new(id)).unwrap()),
                expected,
                "{id}"
            );
        }
        for id in ["child", "grandchild"] {
            select(&mut app, id);
            let selected = app.tree_rows[app.selected_index].clone();
            app.tree_rows = vec![selected];
            app.selected_index = 0;
            assert!(!app.is_coordinator(app.selected_session().unwrap()));
            assert_eq!(app.toggle_selected_coordinator(), None);
            assert_eq!(app.coordinator_panes.len(), 3);
        }
    }

    #[test]
    fn coordinator_toggle_is_immediate_and_ignores_ineligible_rows() {
        let mut app = sessions_app();
        app.update_sessions(vec![
            role_session("lead", Some("%1"), None, "1"),
            role_session("child", Some("%1"), Some("lead"), "2"),
            role_session("mate", Some("%2"), Some("lead"), "3"),
            role_session("new", Some("%3"), None, "4"),
            role_session("no-pane", None, None, "5"),
        ]);
        assert_eq!(app.toggle_selected_coordinator(), None, "group row");
        for id in ["child", "no-pane"] {
            select(&mut app, id);
            assert_eq!(app.toggle_selected_coordinator(), None);
            assert!(app.coordinator_panes.is_empty());
        }
        select(&mut app, "lead");
        assert_eq!(app.toggle_selected_coordinator(), Some(("%1".into(), true)));
        assert_eq!(
            role_row_ids(&app),
            ["lead", "child", "mate", "no-pane", "new"]
        );
        assert_eq!(app.selected_session().unwrap().id.as_str(), "lead");
        assert_eq!(
            app.toggle_selected_coordinator(),
            Some(("%1".into(), false))
        );
        assert!(app.coordinator_panes.is_empty());
        assert_eq!(
            role_row_ids(&app),
            ["no-pane", "new", "lead", "child", "mate"]
        );
        select(&mut app, "mate");
        assert_eq!(app.toggle_selected_coordinator(), Some(("%2".into(), true)));
        assert!(app.is_coordinator(app.selected_session().unwrap()));
        assert_eq!(
            role_row_ids(&app),
            ["no-pane", "new", "lead", "child", "mate"]
        );
    }

    #[test]
    fn filters_keep_same_pane_parents_and_promote_only_separate_pane_teammates() {
        let lead = role_session("lead", Some("%1"), None, "1");
        let child = role_session("child", Some("%1"), Some("lead"), "2");
        let mate = role_session("mate", Some("%2"), Some("lead"), "3");
        let newer = role_session("newer", Some("%3"), None, "4");
        let mut app = App::with_tmux_session_filter("private".into());
        app.view = AppView::Sessions;
        app.update_filter_panes(HashSet::from(["%1".into(), "%2".into(), "%3".into()]));
        app.update_sessions(vec![lead, child, mate, newer]);
        app.update_coordinator_panes(HashSet::from(["%1".into(), "%2".into()]));
        app.start_search();
        for c in "child".chars() {
            app.push_search_char(c);
        }
        assert_eq!(role_row_ids(&app), ["lead", "child"]);
        assert!(!app.is_coordinator(app.sessions.get(&SessionId::new("child")).unwrap()));
        app.clear_filter();
        app.update_filter_panes(HashSet::from(["%2".into(), "%3".into()]));
        assert_eq!(role_row_ids(&app), ["mate", "newer"]);
        assert!(app.sessions.contains_key(&SessionId::new("lead")));
        app.remove_session("lead");
        assert_eq!(role_row_ids(&app), ["mate", "newer"]);
    }

    fn beads_task(id: &str) -> BeadsTask {
        BeadsTask {
            id: id.to_string(),
            title: format!("{id} title"),
            description: None,
        }
    }

    fn select(app: &mut App, id: &str) {
        app.expand_all();
        app.selected_index = app
            .tree_rows
            .iter()
            .position(
                |r| matches!(&r.kind, TreeRowKind::Agent { session } if session.id.as_str() == id),
            )
            .unwrap();
    }

    fn selected_beads_id(app: &App) -> Option<&str> {
        app.selected_beads_task().map(|t| t.id.as_str())
    }

    /// Two sessions in different projects: `aaa` in /p/alpha, `bbb` in /p/beta.
    fn two_project_app() -> App {
        let mut a = create_test_session("aaa", "2024-01-15T10:00:00Z");
        a.project_root = Some("/p/alpha".to_string());
        let mut b = create_test_session("bbb", "2024-01-15T10:01:00Z");
        b.project_root = Some("/p/beta".to_string());
        let mut app = App::new();
        app.update_sessions(vec![a, b]);
        app
    }

    #[test]
    fn test_beads_task_follows_selected_sessions_dir() {
        let mut app = two_project_app();
        app.update_beads_task("/p/alpha".to_string(), Some(beads_task("alpha-1")));

        select(&mut app, "aaa");
        assert_eq!(app.selected_beads_dir(), Some("/p/alpha"));
        assert_eq!(selected_beads_id(&app), Some("alpha-1"));

        select(&mut app, "bbb");
        assert_eq!(app.selected_beads_dir(), Some("/p/beta"));
        assert_eq!(selected_beads_id(&app), None, "beta has not been read yet");
    }

    #[test]
    fn test_beads_update_replaces_only_that_dirs_entry() {
        let mut app = two_project_app();
        app.update_beads_task("/p/alpha".to_string(), Some(beads_task("alpha-1")));
        app.update_beads_task("/p/beta".to_string(), Some(beads_task("beta-1")));

        // A read that found no in-progress task (or no file) clears alpha only.
        app.update_beads_task("/p/alpha".to_string(), None);
        select(&mut app, "aaa");
        assert_eq!(selected_beads_id(&app), None);
        select(&mut app, "bbb");
        assert_eq!(selected_beads_id(&app), Some("beta-1"));

        app.update_beads_task("/p/beta".to_string(), Some(beads_task("beta-2")));
        assert_eq!(selected_beads_id(&app), Some("beta-2"));
    }

    #[test]
    fn test_beads_dir_prefers_project_root_then_worktree() {
        let mut s = create_test_session("aaa", "2024-01-15T10:00:00Z");
        s.project_root = Some("/p/root".to_string());
        s.worktree_path = Some("/p/root/.worktrees/x".to_string());
        let mut app = App::new();
        app.update_sessions(vec![s.clone()]);
        select(&mut app, "aaa");
        assert_eq!(app.selected_beads_dir(), Some("/p/root"));

        s.project_root = None;
        app.update_sessions(vec![s.clone()]);
        select(&mut app, "aaa");
        assert_eq!(app.selected_beads_dir(), Some("/p/root/.worktrees/x"));

        s.worktree_path = None;
        app.update_sessions(vec![s]);
        select(&mut app, "aaa");
        assert_eq!(app.selected_beads_dir(), None);
    }

    fn type_query(app: &mut App, query: &str) {
        app.start_search();
        for c in query.chars() {
            app.push_search_char(c);
        }
    }

    fn agent_ids(app: &App) -> Vec<&str> {
        app.tree_rows
            .iter()
            .filter_map(|r| match &r.kind {
                TreeRowKind::Agent { session } => Some(session.id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_filter_keeps_lead_as_context_for_matching_child() {
        let lead = create_test_session("lead", "2024-01-15T10:00:00Z");
        let mut mate = create_test_session("mate", "2024-01-15T10:01:00Z");
        mate.parent_session_id = Some(lead.id.clone());
        mate.harness = "codex".to_string();
        let other = create_test_session("other", "2024-01-15T10:02:00Z");
        let mut app = sessions_app();
        app.update_sessions(vec![lead, mate, other]);

        type_query(&mut app, "codex");

        assert_eq!(agent_ids(&app), vec!["lead", "mate"]);
        let mate_row = app.tree_rows.iter().find(
            |r| matches!(&r.kind, TreeRowKind::Agent { session } if session.id.as_str() == "mate"),
        );
        assert_eq!(mate_row.map(|r| r.depth), Some(2), "mate stays nested");
        assert!(app.is_filter_context(&SessionId::new("lead")));
        assert!(!app.is_filter_context(&SessionId::new("mate")));
        assert_eq!(
            app.selected_session().map(|s| s.id.as_str()),
            Some("mate"),
            "first match is selected"
        );
    }

    #[test]
    fn test_filter_expands_collapsed_groups_holding_matches() {
        let mut app = sessions_app();
        app.update_sessions(vec![create_test_session("s1", "2024-01-15T10:00:00Z")]);
        app.collapse_all();
        assert!(agent_ids(&app).is_empty());

        type_query(&mut app, "s1");

        assert_eq!(agent_ids(&app), vec!["s1"]);
    }

    #[test]
    fn test_clear_filter_restores_all_sessions() {
        let mut app = sessions_app();
        app.update_sessions(vec![
            create_test_session("aaa", "2024-01-15T10:00:00Z"),
            create_test_session("bbb", "2024-01-15T10:01:00Z"),
        ]);
        type_query(&mut app, "aaa");
        app.confirm_search();
        assert!(!app.search_active);
        assert_eq!(agent_ids(&app), vec!["aaa"]);

        app.clear_filter();

        assert!(app.filter_matches.is_none());
        assert!(!app.is_filter_context(&SessionId::new("aaa")));
        assert_eq!(agent_ids(&app).len(), 2);
        assert_eq!(
            app.selected_session().map(|s| s.id.as_str()),
            Some("aaa"),
            "cursor stays on the session picked while filtered"
        );
    }

    #[test]
    fn test_clear_filter_restores_folds_but_keeps_selection_visible() {
        let mut a = create_test_session("aaa", "2024-01-15T10:00:00Z");
        a.project_root = Some("/p/alpha".to_string());
        let mut b = create_test_session("bbb", "2024-01-15T10:01:00Z");
        b.project_root = Some("/p/beta".to_string());
        let mut app = sessions_app();
        app.update_sessions(vec![a, b]);
        app.collapse_all();
        assert!(agent_ids(&app).is_empty());

        type_query(&mut app, "aaa");
        assert_eq!(agent_ids(&app), vec!["aaa"]);
        app.clear_filter();

        // alpha holds the selection so it stays open; beta is collapsed again.
        assert_eq!(agent_ids(&app), vec!["aaa"]);
        assert!(!app
            .expanded
            .contains(&TreeNodeId::Project("/p/beta".to_string())));
        assert_eq!(app.selected_session().map(|s| s.id.as_str()), Some("aaa"));
    }

    #[test]
    fn test_clear_filter_reopens_worktree_hidden_while_filtered() {
        // Two worktrees: filtering to one drops the worktree level (single
        // worktree), so the collapsed worktree never appears while filtered.
        let wt = |id: &str, path: &str, t: &str| {
            let mut s = create_test_session(id, t);
            s.project_root = Some("/p".to_string());
            s.worktree_path = Some(path.to_string());
            s
        };
        let mut app = sessions_app();
        app.update_sessions(vec![
            wt("aaa", "/p", "2024-01-15T10:00:00Z"),
            wt("bbb", "/p/.worktrees/feat", "2024-01-15T10:01:00Z"),
        ]);
        let feat = TreeNodeId::Worktree("/p/.worktrees/feat".to_string());
        app.set_node_expanded(&feat, false);
        assert_eq!(agent_ids(&app), vec!["aaa"]);

        type_query(&mut app, "bbb");
        app.clear_filter();

        assert_eq!(app.selected_session().map(|s| s.id.as_str()), Some("bbb"));
        assert!(app.expanded.contains(&feat));
    }

    #[test]
    fn test_backspace_to_empty_restores_folds() {
        let mut a = create_test_session("aaa", "2024-01-15T10:00:00Z");
        a.project_root = Some("/p/alpha".to_string());
        let mut b = create_test_session("bbb", "2024-01-15T10:01:00Z");
        b.project_root = Some("/p/beta".to_string());
        let mut app = sessions_app();
        app.update_sessions(vec![a, b]);
        let beta = TreeNodeId::Project("/p/beta".to_string());
        app.set_node_expanded(&beta, false);

        type_query(&mut app, "bbb");
        assert!(app.expanded.contains(&beta), "filter opens beta");
        app.pop_search_char();
        app.pop_search_char(); // "b": each edit re-selects the first match
        app.select_go_to(0); // cursor on beta's own row, not inside it
        app.pop_search_char();

        assert!(app.filter_matches.is_none());
        assert!(!app.expanded.contains(&beta), "beta collapsed again");
    }

    #[test]
    fn test_backspace_to_empty_removes_filter() {
        let mut app = sessions_app();
        app.update_sessions(vec![create_test_session("aaa", "2024-01-15T10:00:00Z")]);
        type_query(&mut app, "zz");
        assert!(agent_ids(&app).is_empty());

        app.pop_search_char();
        app.pop_search_char();
        app.pop_search_char(); // extra pop on empty query is a no-op

        assert!(app.search_active, "prompt stays open");
        assert!(app.filter_matches.is_none());
        assert_eq!(agent_ids(&app), vec!["aaa"]);
    }

    #[test]
    fn test_filter_applies_to_sessions_arriving_later() {
        let mut app = sessions_app();
        type_query(&mut app, "codex");
        let mut late = create_test_session("late", "2024-01-15T10:00:00Z");
        late.harness = "codex".to_string();

        app.update_sessions(vec![
            late,
            create_test_session("plain", "2024-01-15T10:01:00Z"),
        ]);

        assert_eq!(agent_ids(&app), vec!["late"]);
    }

    #[test]
    fn test_filter_opens_collapsed_group_when_session_starts_matching() {
        let mut a = create_test_session("aaa", "2024-01-15T10:00:00Z");
        a.project_root = Some("/p/alpha".to_string());
        a.harness = "qq".to_string();
        let mut b = create_test_session("bbb", "2024-01-15T10:01:00Z");
        b.project_root = Some("/p/beta".to_string());
        let mut app = sessions_app();
        app.update_sessions(vec![a, b.clone()]);
        app.set_node_expanded(&TreeNodeId::Project("/p/beta".to_string()), false);
        // No prefix of "qq" matches bbb, so typing never opens beta.
        type_query(&mut app, "qq");
        app.confirm_search();

        b.harness = "qq".to_string();
        app.update_sessions(vec![b]);

        assert_eq!(agent_ids(&app), vec!["aaa", "bbb"]);
    }

    #[test]
    fn test_filter_opens_worktree_level_that_appears_around_existing_match() {
        // One matching worktree hides the worktree level; a second match in
        // another worktree brings it back around the existing match too.
        let wt = |id: &str, path: &str, t: &str| {
            let mut s = create_test_session(id, t);
            s.project_root = Some("/p".to_string());
            s.worktree_path = Some(path.to_string());
            s
        };
        let mut a = wt("aaa", "/p", "2024-01-15T10:00:00Z");
        a.harness = "qq".to_string();
        let mut b = wt("bbb", "/p/.worktrees/feat", "2024-01-15T10:01:00Z");
        let mut app = sessions_app();
        app.update_sessions(vec![a, b.clone()]);
        app.collapse_all();
        type_query(&mut app, "qq");
        assert_eq!(agent_ids(&app), vec!["aaa"]);

        b.harness = "qq".to_string();
        app.update_sessions(vec![b]);

        assert_eq!(agent_ids(&app), vec!["aaa", "bbb"]);
    }

    #[test]
    fn test_filter_opens_new_group_of_match_that_moved_project() {
        let mut a = create_test_session("aaa", "2024-01-15T10:00:00Z");
        a.project_root = Some("/p/alpha".to_string());
        a.harness = "qq".to_string();
        let mut b = create_test_session("bbb", "2024-01-15T10:01:00Z");
        b.project_root = Some("/p/beta".to_string());
        let mut app = sessions_app();
        app.update_sessions(vec![a.clone(), b]);
        app.set_node_expanded(&TreeNodeId::Project("/p/beta".to_string()), false);
        type_query(&mut app, "qq");

        a.project_root = Some("/p/beta".to_string());
        app.update_sessions(vec![a]);

        assert_eq!(agent_ids(&app), vec!["aaa"]);
    }

    #[test]
    fn test_clear_filter_after_zero_results_keeps_selection() {
        let mut app = sessions_app();
        app.update_sessions(vec![
            create_test_session("aaa", "2024-01-15T10:00:00Z"),
            create_test_session("bbb", "2024-01-15T10:01:00Z"),
        ]);
        type_query(&mut app, "bbb");
        app.push_search_char('z');
        assert!(agent_ids(&app).is_empty());

        app.clear_filter();

        assert_eq!(app.selected_session().map(|s| s.id.as_str()), Some("bbb"));
    }

    #[test]
    fn test_clear_filter_started_before_sessions_loaded_opens_all() {
        let mut a = create_test_session("aaa", "2024-01-15T10:00:00Z");
        a.project_root = Some("/p/alpha".to_string());
        let mut b = create_test_session("bbb", "2024-01-15T10:01:00Z");
        b.project_root = Some("/p/beta".to_string());
        let mut app = sessions_app();
        type_query(&mut app, "aaa");
        app.update_sessions(vec![a, b]);

        app.clear_filter();

        assert_eq!(agent_ids(&app), vec!["aaa", "bbb"]);
    }

    #[test]
    fn test_app_new_is_connecting() {
        let app = App::new();
        assert_eq!(app.state, AppState::Connecting);
        assert!(app.sessions.is_empty());
        assert_eq!(app.selected_index, 0);
        assert!(!app.should_quit);
        assert!(app.blink_visible);
    }

    #[test]
    fn test_app_default_equals_new() {
        let app1 = App::new();
        let app2 = App::default();
        assert_eq!(app1.state, app2.state);
        assert_eq!(app1.sessions.len(), app2.sessions.len());
        assert_eq!(app1.should_quit, app2.should_quit);
    }

    #[test]
    fn test_update_sessions_marks_connected() {
        let mut app = App::new();
        assert_eq!(app.state, AppState::Connecting);

        let sessions = vec![create_test_session("session-1", "2024-01-15T10:00:00Z")];
        app.update_sessions(sessions);

        assert_eq!(app.state, AppState::Connected);
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn test_update_sessions_clamps_selection() {
        let mut app = App::new();
        app.selected_index = 50;

        let sessions = vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
        ];
        app.update_sessions(sessions);

        // Tree rows: 1 project header + 2 agents = 3 rows, max index = 2
        assert_eq!(app.selected_index, app.tree_rows.len() - 1);
    }

    #[test]
    fn test_update_sessions_empty_resets_selection() {
        let mut app = App::new();
        app.selected_index = 3;

        app.update_sessions(vec![]);

        assert_eq!(app.selected_index, 0);
        assert!(app.sessions.is_empty());
    }

    #[test]
    fn test_remove_session_removes_by_id() {
        let mut app = App::new();
        let sessions = vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
            create_test_session("session-3", "2024-01-15T10:02:00Z"),
        ];
        app.update_sessions(sessions);
        assert_eq!(app.sessions.len(), 3);

        app.remove_session("session-2");

        assert_eq!(app.sessions.len(), 2);
        assert!(app.sessions.contains_key(&SessionId::new("session-1")));
        assert!(!app.sessions.contains_key(&SessionId::new("session-2")));
        assert!(app.sessions.contains_key(&SessionId::new("session-3")));
    }

    #[test]
    fn test_remove_session_clamps_selection() {
        let mut app = App::new();
        let sessions = vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
        ];
        app.update_sessions(sessions);
        let last_idx = app.tree_rows.len() - 1;
        app.selected_index = last_idx;

        app.remove_session("session-2");

        // Selection should be clamped within new tree_rows bounds
        assert!(app.selected_index < app.tree_rows.len());
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn test_remove_session_nonexistent() {
        let mut app = App::new();
        let sessions = vec![create_test_session("session-1", "2024-01-15T10:00:00Z")];
        app.update_sessions(sessions);

        // Removing non-existent session should not panic or change anything
        app.remove_session("nonexistent-session");

        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions.contains_key(&SessionId::new("session-1")));
    }

    #[test]
    fn test_remove_session_empty_to_empty() {
        let mut app = App::new();
        app.remove_session("any-session");
        assert!(app.sessions.is_empty());
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_mark_disconnected_from_connected() {
        let mut app = App::new();
        app.state = AppState::Connected;

        app.mark_disconnected(None);

        match &app.state {
            AppState::Disconnected { retry_count, .. } => {
                assert_eq!(*retry_count, 1);
            }
            _ => panic!("Expected Disconnected state"),
        }
    }

    #[test]
    fn test_mark_disconnected_increments_retry() {
        let mut app = App::new();
        app.state = AppState::Disconnected {
            since: Utc::now(),
            retry_count: 3,
            reason: None,
        };

        app.mark_disconnected(None);

        match &app.state {
            AppState::Disconnected { retry_count, .. } => {
                assert_eq!(*retry_count, 4);
            }
            _ => panic!("Expected Disconnected state"),
        }
    }

    #[test]
    fn test_sessions_sorted_newest_first() {
        let mut app = App::new();
        let sessions = vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:02:00Z"),
            create_test_session("session-3", "2024-01-15T10:01:00Z"),
        ];
        app.update_sessions(sessions);

        let sorted = app.sessions_sorted();
        assert_eq!(sorted.len(), 3);
        // Newest first: session-2, session-3, session-1
        assert_eq!(sorted.first().map(|s| s.id.as_str()), Some("session-2"));
        assert_eq!(sorted.get(1).map(|s| s.id.as_str()), Some("session-3"));
        assert_eq!(sorted.get(2).map(|s| s.id.as_str()), Some("session-1"));
    }

    #[test]
    fn test_selected_session_returns_correct_session() {
        let mut app = App::new();
        let sessions = vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:02:00Z"),
        ];
        app.update_sessions(sessions);

        // Index 0 is the project header — selected_session returns None
        app.selected_index = 0;
        assert!(app.selected_session().is_none());

        // Index 1 is the first agent (newest: session-2)
        app.selected_index = 1;
        assert_eq!(
            app.selected_session().map(|s| s.id.as_str()),
            Some("session-2")
        );

        // Index 2 is the second agent (session-1)
        app.selected_index = 2;
        assert_eq!(
            app.selected_session().map(|s| s.id.as_str()),
            Some("session-1")
        );
    }

    #[test]
    fn test_selected_session_none_when_empty() {
        let app = App::new();
        assert!(app.selected_session().is_none());
    }

    #[test]
    fn test_select_next_wraps_around() {
        let mut app = App::new();
        let sessions = vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
        ];
        app.update_sessions(sessions);
        // Tree: [Project, Agent, Agent] = 3 rows
        let last = app.tree_rows.len() - 1;

        app.selected_index = last;
        app.select_next();
        assert_eq!(app.selected_index, 0); // Wrapped around
    }

    #[test]
    fn test_select_previous_wraps_around() {
        let mut app = App::new();
        let sessions = vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
        ];
        app.update_sessions(sessions);
        let last = app.tree_rows.len() - 1;

        assert_eq!(app.selected_index, 0);
        app.select_previous();
        assert_eq!(app.selected_index, last); // Wrapped to last
    }

    #[test]
    fn test_select_next_empty_sessions() {
        let mut app = App::new();
        app.select_next();
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_select_previous_empty_sessions() {
        let mut app = App::new();
        app.select_previous();
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_tick_blink_timing() {
        let mut app = App::new();
        assert!(app.blink_visible); // Starts visible

        // Ticks 1-4: no change
        for _ in 0..4 {
            app.tick();
            assert!(app.blink_visible);
        }

        // Tick 5: toggles to not visible
        app.tick();
        assert!(!app.blink_visible);

        // Ticks 6-9: no change
        for _ in 0..4 {
            app.tick();
            assert!(!app.blink_visible);
        }

        // Tick 10: toggles back to visible
        app.tick();
        assert!(app.blink_visible);
    }

    #[test]
    fn test_quit() {
        let mut app = App::new();
        assert!(!app.should_quit);

        app.quit();
        assert!(app.should_quit);
    }

    #[test]
    fn test_session_count() {
        let mut app = App::new();
        assert_eq!(app.session_count(), 0);

        let sessions = vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
        ];
        app.update_sessions(sessions);
        assert_eq!(app.session_count(), 2);
    }

    #[test]
    fn test_total_cost() {
        let mut app = App::new();
        assert_eq!(app.total_cost(), 0.0);

        let mut session1 = create_test_session("session-1", "2024-01-15T10:00:00Z");
        session1.cost_usd = 1.50;
        let mut session2 = create_test_session("session-2", "2024-01-15T10:01:00Z");
        session2.cost_usd = 2.25;

        app.update_sessions(vec![session1, session2]);
        assert!((app.total_cost() - 3.75).abs() < 0.001);
    }

    #[test]
    fn test_average_context_empty() {
        let app = App::new();
        assert_eq!(app.average_context(), 0.0);
    }

    #[test]
    fn test_average_context() {
        let mut app = App::new();

        let mut session1 = create_test_session("session-1", "2024-01-15T10:00:00Z");
        session1.context_percentage = 20.0;
        let mut session2 = create_test_session("session-2", "2024-01-15T10:01:00Z");
        session2.context_percentage = 40.0;

        app.update_sessions(vec![session1, session2]);
        assert!((app.average_context() - 30.0).abs() < 0.001);
    }

    #[test]
    fn test_attention_count() {
        let mut app = App::new();
        assert_eq!(app.attention_count(), 0);

        let mut session1 = create_test_session("session-1", "2024-01-15T10:00:00Z");
        session1.needs_attention = true;
        let session2 = create_test_session("session-2", "2024-01-15T10:01:00Z");

        app.update_sessions(vec![session1, session2]);
        assert_eq!(app.attention_count(), 1);
    }

    #[test]
    fn test_working_count() {
        let mut app = App::new();
        assert_eq!(app.working_count(), 0);

        let session1 = create_test_session("session-1", "2024-01-15T10:00:00Z");
        // session1 has status = Working by default
        let mut session2 = create_test_session("session-2", "2024-01-15T10:01:00Z");
        session2.status = atm_core::SessionStatus::Idle;

        app.update_sessions(vec![session1, session2]);
        assert_eq!(app.working_count(), 1);
    }

    // ------------------------------------------------------------------------
    // Vim navigation clamping tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_select_down_empty_list() {
        let mut app = App::new();
        app.select_down(5);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_select_down_clamps_at_last() {
        let mut app = App::new();
        app.update_sessions(vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
            create_test_session("session-3", "2024-01-15T10:02:00Z"),
        ]);
        let last = app.tree_rows.len() - 1;
        app.selected_index = 1;
        app.select_down(100);
        assert_eq!(app.selected_index, last);
    }

    #[test]
    fn test_select_up_clamps_at_first() {
        let mut app = App::new();
        app.update_sessions(vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
        ]);
        app.selected_index = 1;
        app.select_up(10);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_select_go_to_clamps_to_len() {
        let mut app = App::new();
        app.update_sessions(vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
        ]);
        let last = app.tree_rows.len() - 1;
        app.select_go_to(99);
        assert_eq!(app.selected_index, last);
    }

    #[test]
    fn test_select_go_to_empty_list() {
        let mut app = App::new();
        app.select_go_to(5);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_half_page_down_with_viewport_20() {
        let mut app = App::new();
        let sessions: Vec<_> = (0..12)
            .map(|i| create_test_session(&format!("s{i}"), &format!("2024-01-15T10:{i:02}:00Z")))
            .collect();
        app.update_sessions(sessions);
        // 13 tree rows (1 project header + 12 agents), half page = 10
        app.selected_index = 0;
        app.select_half_page_down(1, 20);
        assert_eq!(app.selected_index, 10);
    }

    #[test]
    fn test_half_page_up_with_viewport_20() {
        let mut app = App::new();
        let sessions: Vec<_> = (0..12)
            .map(|i| create_test_session(&format!("s{i}"), &format!("2024-01-15T10:{i:02}:00Z")))
            .collect();
        app.update_sessions(sessions);
        app.selected_index = 12; // last agent row
        app.select_half_page_up(1, 20);
        assert_eq!(app.selected_index, 2);
    }

    // ------------------------------------------------------------------------
    // Help popup tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_show_help_default_false() {
        let app = App::new();
        assert!(!app.show_help);
    }

    #[test]
    fn test_toggle_help() {
        let mut app = App::new();
        assert!(!app.show_help);
        app.toggle_help();
        assert!(app.show_help);
        app.toggle_help();
        assert!(!app.show_help);
    }

    #[test]
    fn test_half_page_zero_viewport_is_noop() {
        let mut app = App::new();
        app.update_sessions(vec![
            create_test_session("session-1", "2024-01-15T10:00:00Z"),
            create_test_session("session-2", "2024-01-15T10:01:00Z"),
        ]);
        app.selected_index = 0;
        app.select_half_page_down(3, 0);
        assert_eq!(app.selected_index, 0);
    }

    // ------------------------------------------------------------------
    // Vim fold commands (zM/zR/zc/zo/za)
    // ------------------------------------------------------------------

    /// Creates a session under a specific project root.
    fn session_in(id: &str, started_at: &str, project_root: &str) -> SessionView {
        let mut s = create_test_session(id, started_at);
        s.project_root = Some(project_root.to_string());
        s
    }

    /// Builds an app with two projects containing two agents each.
    /// Resulting tree (fully expanded): proj-a, agent, agent, proj-b, agent, agent.
    fn app_two_projects() -> App {
        let mut app = sessions_app();
        app.update_sessions(vec![
            session_in("sess-a1", "2024-01-15T10:00:00Z", "/repos/proj-a"),
            session_in("sess-a2", "2024-01-15T10:01:00Z", "/repos/proj-a"),
            session_in("sess-b1", "2024-01-15T10:02:00Z", "/repos/proj-b"),
            session_in("sess-b2", "2024-01-15T10:03:00Z", "/repos/proj-b"),
        ]);
        app
    }

    /// `app_two_projects` with `ids` flagged as needing attention.
    fn app_with_attention(ids: &[&str]) -> App {
        let mut app = app_two_projects();
        let flagged: Vec<SessionView> = app
            .sessions
            .values()
            .filter(|s| ids.contains(&s.id.as_str()))
            .map(|s| SessionView {
                needs_attention: true,
                ..s.clone()
            })
            .collect();
        app.update_sessions(flagged);
        app
    }

    fn selected_id(app: &App) -> Option<&str> {
        app.selected_session().map(|s| s.id.as_str())
    }

    // Break caught: waiting decisions do not participate in n/N, mark shared-pane
    // subagents, escape a filter, or lose the selection when marks refresh.
    #[test]
    fn waiting_decision_navigation_opens_folds_and_excludes_shared_pane_children() {
        let mut app = App::new();
        app.update_sessions(vec![
            role_session("parent", Some("%7"), None, "1"),
            role_session("child", Some("%7"), Some("parent"), "2"),
            role_session("other", Some("%8"), None, "3"),
        ]);
        app.decision_panes.insert("%7".into());
        app.collapse_all();
        app.select_attention(true);
        assert_eq!(selected_id(&app), Some("parent"));
        let before = app.selected_index;
        app.select_attention(true);
        app.select_attention(false);
        assert_eq!(selected_id(&app), Some("parent"));
        assert_eq!(before, app.selected_index);
        assert!(!app.has_waiting_decision(app.sessions.get(&SessionId::new("child")).unwrap()));
        app.decision_panes.clear();
        app.select_attention(true);
        assert_eq!(before, app.selected_index);
        app.decision_panes.insert("%7".into());
        app.start_search();
        for c in "other".chars() {
            app.push_search_char(c);
        }
        app.confirm_search();
        let filtered = app.selected_index;
        app.select_attention(true);
        assert_eq!(filtered, app.selected_index);
        assert!(!app.tree_rows.iter().any(
            |row| matches!(&row.kind, TreeRowKind::Agent {session} if session.id.as_str()=="parent")
        ));
    }

    #[test]
    fn test_select_attention_forward_wraps() {
        let mut app = app_with_attention(&["sess-a1", "sess-b2"]);
        app.select_go_to(0);
        app.select_attention(true);
        let first = selected_id(&app).map(str::to_string);
        app.select_attention(true);
        let second = selected_id(&app).map(str::to_string);
        app.select_attention(true);
        assert_eq!(selected_id(&app).map(str::to_string), first);
        let mut seen = vec![first, second];
        seen.sort();
        assert_eq!(
            seen,
            vec![Some("sess-a1".to_string()), Some("sess-b2".to_string())]
        );
    }

    #[test]
    fn test_select_attention_backward_reverses_forward() {
        let mut app = app_with_attention(&["sess-a1", "sess-a2", "sess-b1"]);
        app.select_go_to(0);
        app.select_attention(true);
        let start = selected_id(&app).map(str::to_string);
        app.select_attention(true);
        app.select_attention(false);
        assert_eq!(selected_id(&app).map(str::to_string), start);
    }

    #[test]
    fn test_select_attention_opens_collapsed_fold() {
        let mut app = app_with_attention(&["sess-b1"]);
        app.collapse_all();
        app.select_go_to(0);
        app.select_attention(true);
        assert_eq!(selected_id(&app), Some("sess-b1"));
    }

    #[test]
    fn test_select_attention_noop_when_none_flagged() {
        let mut app = app_with_attention(&[]);
        app.select_go_to(2);
        let rows = app.tree_rows.len();
        app.select_attention(true);
        app.select_attention(false);
        assert_eq!(app.selected_index, 2);
        assert_eq!(app.tree_rows.len(), rows);
    }

    #[test]
    fn test_select_attention_on_empty_app_is_noop() {
        let mut app = App::new();
        app.select_attention(true);
        app.select_attention(false);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn test_collapse_all_shows_only_top_level_rows() {
        let mut app = app_two_projects();
        assert!(app.tree_rows.len() > 2, "tree should start expanded");

        app.collapse_all();

        // Only the two project rows remain, and both are at depth 0.
        assert_eq!(app.tree_rows.len(), 2);
        assert!(app.tree_rows.iter().all(|r| r.depth == 0));
    }

    #[test]
    fn test_collapse_all_reselects_top_level_ancestor() {
        let mut app = app_two_projects();
        // Find an agent row in proj-b and select it.
        let (agent_idx, agent_id) = app
            .tree_rows
            .iter()
            .enumerate()
            .find_map(|(i, r)| match &r.node_id {
                TreeNodeId::Agent(id) if id.as_str() == "sess-b1" => Some((i, r.node_id.clone())),
                _ => None,
            })
            .expect("sess-b1 must be in tree");
        let _ = agent_id;
        app.selected_index = agent_idx;

        app.collapse_all();

        // Selection should now point at the proj-b project row.
        let selected = &app.tree_rows[app.selected_index];
        match &selected.node_id {
            TreeNodeId::Project(root) => assert_eq!(root, "/repos/proj-b"),
            other => panic!("expected proj-b project, got {other:?}"),
        }
    }

    #[test]
    fn test_expand_all_populates_expanded_set() {
        let mut app = app_two_projects();
        app.collapse_all();
        assert_eq!(app.tree_rows.len(), 2);

        app.expand_all();

        // Tree should be back to fully-expanded size (2 projects + 4 agents).
        assert_eq!(app.tree_rows.len(), 6);
    }

    // ------------------------------------------------------------------
    // open_fold (h / l / zo)
    // ------------------------------------------------------------------

    #[test]
    fn test_open_fold_expands_collapsed_group() {
        let mut app = app_two_projects();
        app.collapse_all();
        app.selected_index = 0;
        let proj_a = TreeNodeId::Project("/repos/proj-a".to_string());

        app.open_fold();

        assert!(app.expanded.contains(&proj_a));
        assert!(app.tree_rows.len() > 2);
    }

    #[test]
    fn test_open_fold_on_expanded_group_is_noop() {
        let mut app = app_two_projects();
        app.selected_index = 0; // expanded proj-a
        let rows_before = app.tree_rows.len();
        let expanded_before = app.expanded.clone();

        app.open_fold();

        assert_eq!(app.tree_rows.len(), rows_before);
        assert_eq!(app.expanded, expanded_before);
    }

    #[test]
    fn test_open_fold_on_leaf_is_noop() {
        let mut app = app_two_projects();
        let agent_idx = app
            .tree_rows
            .iter()
            .position(|r| matches!(r.node_id, TreeNodeId::Agent(_)))
            .expect("at least one agent row");
        app.selected_index = agent_idx;
        let expanded_before = app.expanded.clone();

        app.open_fold();

        assert_eq!(app.expanded, expanded_before);
    }

    // ------------------------------------------------------------------
    // close_fold (zc) — vim-strict directional semantics
    // ------------------------------------------------------------------

    #[test]
    fn test_close_fold_closes_expanded_group() {
        let mut app = app_two_projects();
        app.selected_index = 0;
        let proj_id = app.tree_rows[0].node_id.clone();

        app.close_fold();

        assert!(!app.expanded.contains(&proj_id));
    }

    #[test]
    fn test_close_fold_on_collapsed_group_is_noop() {
        // Vim: `zc` on an already-closed fold does nothing.
        let mut app = app_two_projects();
        app.collapse_all();
        app.selected_index = 0;
        let rows_before = app.tree_rows.len();
        let expanded_before = app.expanded.clone();

        app.close_fold();

        assert_eq!(app.tree_rows.len(), rows_before);
        assert_eq!(app.expanded, expanded_before);
    }

    #[test]
    fn test_close_fold_on_leaf_walks_up_to_parent() {
        let mut app = app_two_projects();
        let agent_idx = app
            .tree_rows
            .iter()
            .position(|r| matches!(&r.node_id, TreeNodeId::Agent(id) if id.as_str() == "sess-a1"))
            .expect("sess-a1 must exist");
        app.selected_index = agent_idx;

        app.close_fold();

        let proj_a = TreeNodeId::Project("/repos/proj-a".to_string());
        assert!(!app.expanded.contains(&proj_a));
        // Selection moved to the collapsed parent.
        match &app.tree_rows[app.selected_index].node_id {
            TreeNodeId::Project(root) => assert_eq!(root, "/repos/proj-a"),
            other => panic!("expected proj-a, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // toggle_fold (za)
    // ------------------------------------------------------------------

    #[test]
    fn test_toggle_fold_on_expanded_group_closes() {
        let mut app = app_two_projects();
        app.selected_index = 0;
        let id = app.tree_rows[0].node_id.clone();

        app.toggle_fold();
        assert!(!app.expanded.contains(&id));
    }

    #[test]
    fn test_toggle_fold_on_collapsed_group_opens() {
        let mut app = app_two_projects();
        app.collapse_all();
        app.selected_index = 0;
        let id = app.tree_rows[0].node_id.clone();

        app.toggle_fold();
        assert!(app.expanded.contains(&id));
    }

    #[test]
    fn test_toggle_fold_on_leaf_toggles_parent() {
        let mut app = app_two_projects();
        let agent_idx = app
            .tree_rows
            .iter()
            .position(|r| matches!(&r.node_id, TreeNodeId::Agent(id) if id.as_str() == "sess-a1"))
            .expect("sess-a1 must exist");
        app.selected_index = agent_idx;
        let proj_a = TreeNodeId::Project("/repos/proj-a".to_string());

        app.toggle_fold();
        assert!(!app.expanded.contains(&proj_a));

        // Selection now on the collapsed parent; toggling reopens it.
        app.toggle_fold();
        assert!(app.expanded.contains(&proj_a));
    }

    // ------------------------------------------------------------------
    // End-to-end: InputHandler → UiAction → App dispatch
    // ------------------------------------------------------------------

    #[test]
    fn test_z_chord_wires_through_end_to_end() {
        // Mirrors the dispatch in src/bin/atm/tui.rs.
        use crate::keybinding::{InputHandler, UiAction};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let dispatch = |app: &mut App, action: UiAction| match action {
            UiAction::CollapseAllFolds => app.collapse_all(),
            UiAction::ExpandAllFolds => app.expand_all(),
            UiAction::ExpandNode => app.open_fold(),
            UiAction::CloseFold => app.close_fold(),
            UiAction::ToggleFold => app.toggle_fold(),
            _ => {}
        };

        let mut app = app_two_projects();
        let mut h = InputHandler::new();
        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let rows_before = app.tree_rows.len();

        // zM collapses everything.
        assert!(h.handle(key('z')).is_none());
        let action = h.handle(key('M')).expect("zM emits action");
        assert_eq!(action, UiAction::CollapseAllFolds);
        dispatch(&mut app, action);
        assert_eq!(app.tree_rows.len(), 2);

        // Cursor on collapsed proj-a; zo → ExpandNode → open_fold → expand.
        app.selected_index = 0;
        assert!(h.handle(key('z')).is_none());
        let action = h.handle(key('o')).expect("zo emits action");
        assert_eq!(action, UiAction::ExpandNode);
        dispatch(&mut app, action);
        let proj_a = TreeNodeId::Project("/repos/proj-a".to_string());
        assert!(app.expanded.contains(&proj_a));

        // zc on the now-expanded proj-a closes it (vim-strict close-only).
        app.selected_index = 0;
        assert!(h.handle(key('z')).is_none());
        let action = h.handle(key('c')).expect("zc emits action");
        assert_eq!(action, UiAction::CloseFold);
        dispatch(&mut app, action);
        assert!(!app.expanded.contains(&proj_a));

        // zR expands everything.
        assert!(h.handle(key('z')).is_none());
        let action = h.handle(key('R')).expect("zR emits action");
        assert_eq!(action, UiAction::ExpandAllFolds);
        dispatch(&mut app, action);
        assert_eq!(app.tree_rows.len(), rows_before);
    }

    /// A capture still in flight when the target moved must not land under
    /// the new selection.
    #[test]
    fn update_capture_drops_output_for_a_pane_no_longer_tracked() {
        let mut app = App::new();
        app.capture_pane_id = Some("%2".to_string());

        app.update_capture("%1", vec!["old pane".to_string()]);
        assert!(app.captured_output.is_empty());

        app.update_capture("%2", vec!["new pane".to_string()]);
        assert_eq!(app.captured_output, vec!["new pane".to_string()]);

        app.capture_pane_id = None;
        app.captured_output.clear();
        app.update_capture("%2", vec!["no selection".to_string()]);
        assert!(app.captured_output.is_empty());
    }
}
