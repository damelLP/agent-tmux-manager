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
/// htop-style monitoring interface.
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

        self.tree_rows = flatten_tree(&self.tree, &self.expanded);
        self.clamp_selection();
    }

    /// Toggles expand/collapse for the currently selected node.
    pub fn toggle_expand(&mut self) {
        if let Some(row) = self.tree_rows.get(self.selected_index) {
            if row.has_children {
                let node_id = row.node_id.clone();
                if self.expanded.contains(&node_id) {
                    self.expanded.remove(&node_id);
                } else {
                    self.expanded.insert(node_id);
                }
                // Re-flatten with new expand state
                self.tree_rows = flatten_tree(&self.tree, &self.expanded);
                self.clamp_selection();
            }
        }
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
}
