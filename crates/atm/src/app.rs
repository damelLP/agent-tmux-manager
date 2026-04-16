//! Application state machine for the ATM TUI.
//!
//! This module defines the core state model for the TUI, including
//! connection state tracking and session management.
//!
//! All code follows the panic-free policy: no `.unwrap()`, `.expect()`,
//! `panic!()`, `unreachable!()`, `todo!()`, or direct indexing `[i]`.

use atm_core::{
    all_node_ids, build_tree, flatten_tree, SessionId, SessionView, TreeNode, TreeNodeId, TreeRow,
    TreeRowKind,
};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

// ============================================================================
// Application State
// ============================================================================

/// Connection state of the TUI to the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum AppState {
    /// Connected to daemon and receiving updates.
    Connected,

    /// Lost connection, attempting reconnect.
    Disconnected {
        /// When the connection was lost.
        since: DateTime<Utc>,
        /// Number of reconnection attempts.
        retry_count: u32,
    },

    /// Initial connection in progress.
    Connecting,
}

impl Default for AppState {
    fn default() -> Self {
        Self::Connecting
    }
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

    /// All active sessions indexed by their ID.
    pub sessions: HashMap<SessionId, SessionView>,

    /// Index of the currently selected session in the sorted list.
    pub selected_index: usize,

    /// Flag indicating the application should quit.
    pub should_quit: bool,

    /// Timestamp of the last data update from the daemon.
    pub last_update: DateTime<Utc>,

    /// Whether blinking status icons are currently visible.
    /// Toggles every 500ms (5 ticks at 100ms tick rate).
    pub blink_visible: bool,

    /// Internal tick counter for blink timing.
    tick_count: u32,

    /// Pick mode: exit after jumping to a session (fzf-style).
    pub pick_mode: bool,

    /// Whether the help popup is currently visible.
    pub show_help: bool,

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

    /// If set, only show sessions whose tmux pane belongs to this tmux session.
    pub tmux_session_filter: Option<String>,

    /// Pane IDs that belong to the filtered tmux session (populated by filter task).
    pub filter_pane_ids: HashSet<String>,

    /// Compact mode: vertical layout optimized for narrow sidebar panes.
    pub compact: bool,
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
            sessions: HashMap::new(),
            selected_index: 0,
            should_quit: false,
            last_update: Utc::now(),
            blink_visible: true,
            tick_count: 0,
            pick_mode: false,
            show_help: false,
            expanded: HashSet::new(),
            tree: Vec::new(),
            tree_rows: Vec::new(),
            captured_output: Vec::new(),
            capture_pane_id: None,
            tmux_session_filter: None,
            filter_pane_ids: HashSet::new(),
            compact: false,
        }
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

    /// Updates the session list with new data from the daemon.
    ///
    /// Merges new sessions with existing ones (upsert behavior).
    /// This allows individual session updates without losing other sessions.
    pub fn update_sessions(&mut self, sessions: Vec<SessionView>) {
        for session in sessions {
            self.sessions.insert(session.id.clone(), session);
        }
        self.state = AppState::Connected;
        self.last_update = Utc::now();
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
        self.last_update = Utc::now();
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
        let sessions: Vec<SessionView> = if self.tmux_session_filter.is_some() {
            if self.filter_pane_ids.is_empty() {
                // Filter is set but pane IDs not loaded yet — show empty
                Vec::new()
            } else {
                self.sessions
                    .values()
                    .filter(|s| {
                        s.tmux_pane
                            .as_ref()
                            .map_or(false, |p| self.filter_pane_ids.contains(p))
                    })
                    .cloned()
                    .collect()
            }
        } else {
            self.sessions.values().cloned().collect()
        };
        self.tree = build_tree(&sessions);

        // On first build, expand everything so the tree starts open
        if self.expanded.is_empty() && !self.tree.is_empty() {
            self.expanded = all_node_ids(&self.tree);
        }

        self.reflatten();
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
    pub fn mark_disconnected(&mut self) {
        match &self.state {
            AppState::Disconnected { since, retry_count } => {
                self.state = AppState::Disconnected {
                    since: *since,
                    retry_count: retry_count.saturating_add(1),
                };
            }
            AppState::Connected | AppState::Connecting => {
                self.state = AppState::Disconnected {
                    since: Utc::now(),
                    retry_count: 1,
                };
            }
        }
    }

    /// Returns sessions sorted by start time (newest first).
    ///
    /// Sessions are ordered by their `started_at` field in descending order,
    /// so the most recently started sessions appear first.
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

    /// Navigates to the next row (downward), wrapping around if needed.
    pub fn select_next(&mut self) {
        let row_count = self.tree_rows.len();
        if row_count == 0 {
            self.selected_index = 0;
            return;
        }
        self.selected_index = (self.selected_index.saturating_add(1)) % row_count;
    }

    /// Navigates to the previous row (upward), wrapping around if needed.
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

    /// Advances the blink animation by one tick.
    ///
    /// Should be called every 100ms (on each event loop tick).
    /// Toggles `blink_visible` every 5 ticks (500ms).
    pub fn tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        // Toggle blink every 5 ticks (500ms at 100ms tick rate)
        if self.tick_count % 5 == 0 {
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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

        app.mark_disconnected();

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
        };

        app.mark_disconnected();

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
        let mut app = App::new();
        app.update_sessions(vec![
            session_in("sess-a1", "2024-01-15T10:00:00Z", "/repos/proj-a"),
            session_in("sess-a2", "2024-01-15T10:01:00Z", "/repos/proj-a"),
            session_in("sess-b1", "2024-01-15T10:02:00Z", "/repos/proj-b"),
            session_in("sess-b2", "2024-01-15T10:03:00Z", "/repos/proj-b"),
        ]);
        app
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
        // Mirrors the dispatch in src/bin/atm.rs.
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
}
