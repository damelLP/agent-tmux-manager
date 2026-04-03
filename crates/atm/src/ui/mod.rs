//! UI rendering module for the ATM TUI.
//!
//! This module provides the complete rendering pipeline for the htop-style
//! session monitoring interface. It orchestrates the layout and individual
//! widget rendering.
//!
//! # Layout Structure
//!
//! ```text
//! +--------------------------------------------------+
//! |  Header: Title and Connection Status             |  <- 3 lines
//! +---------------+----------------------------------+
//! | Session List  |  Detail Panel                    |  <- fills remaining
//! | (30%)         |  (70%)                           |
//! |  > Session 1  |  Status: running                 |
//! |    Session 2  |  Context: 45% [====    ]         |
//! |    Session 3  |  Duration: 5m                    |
//! +---------------+----------------------------------+
//! |  Footer: Keybinding Hints                        |  <- 3 lines
//! +--------------------------------------------------+
//! ```
//!
//! The detail panel always shows the selected session's details.

pub mod detail_panel;
pub mod help_popup;
pub mod layout;
pub mod session_list;
pub mod status_bar;
pub mod theme;

use crate::app::App;
use layout::AppLayout;
use ratatui::Frame;

// Re-export commonly used items
pub use detail_panel::render_detail_panel_inline;
pub use session_list::render_session_list;
pub use status_bar::{render_footer, render_header};

/// Renders the complete TUI interface.
///
/// This is the main entry point for rendering. It:
/// 1. Creates the split layout (header, list|detail, footer)
/// 2. Renders the header with connection status
/// 3. Renders the session list in the left panel (30%)
/// 4. Renders the detail panel in the right panel (70%)
/// 5. Renders the footer with keybinding hints
///
/// # Arguments
/// * `frame` - The ratatui frame to render into
/// * `app` - The application state containing session data and UI state
///
/// # Example
///
/// ```ignore
/// terminal.draw(|frame| {
///     ui::render(frame, &app);
/// })?;
/// ```
pub fn render(frame: &mut Frame, app: &App) {
    // Create the split layout
    let layout = AppLayout::new(frame.area());

    // Render header and footer
    render_header(frame, layout.header, app);
    render_footer(frame, layout.footer, app);

    // Render split view: session list (30%) | detail panel (70%)
    render_session_list(frame, layout.list_area, app);
    render_detail_panel_inline(frame, layout.detail_area, app.selected_session());

    // Render help popup overlay (on top of everything)
    if app.show_help {
        help_popup::render_help_popup(frame, frame.area());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppState};
    use atm_core::{SessionId, SessionView};
    use ratatui::{backend::TestBackend, Terminal};

    /// Helper to create a test session for rendering tests
    fn create_test_session(id: &str) -> SessionView {
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
            context_percentage: 45.0,
            context_display: "45%".to_string(),
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
            started_at: "2024-01-15T10:00:00Z".to_string(),
            last_activity: "2024-01-15T10:05:00Z".to_string(),
            tmux_pane: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_render_empty_state() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new();

        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();

        // Verify the render completed without panic
        // The test backend allows us to inspect the buffer if needed
    }

    #[test]
    fn test_render_with_sessions() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = App::new();
        app.state = AppState::Connected;
        app.update_sessions(vec![
            create_test_session("session-1-abcdef"),
            create_test_session("session-2-ghijkl"),
        ]);

        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
    }

    #[test]
    fn test_render_with_split_layout() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = App::new();
        app.state = AppState::Connected;
        app.update_sessions(vec![create_test_session("session-1-abcdef")]);

        // Detail panel is always visible in split layout
        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
    }

    #[test]
    fn test_render_with_help_popup_80x24() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = App::new();
        app.state = AppState::Connected;
        app.show_help = true;
        app.update_sessions(vec![create_test_session("session-1-abcdef")]);

        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
    }

    #[test]
    fn test_render_with_help_popup_40x12() {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = App::new();
        app.show_help = true;

        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
    }

    #[test]
    fn test_render_disconnected_state() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = App::new();
        app.mark_disconnected();

        terminal
            .draw(|frame| {
                render(frame, &app);
            })
            .unwrap();
    }
}
