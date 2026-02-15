//! Layout helpers for the ATM TUI.
//!
//! Provides functions for creating the main application layout
//! and utility functions for popup positioning.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Main application layout areas.
///
/// The TUI is divided into three vertical sections:
/// - Header (3 lines): Title and connection status
/// - Content (fills remaining): Split into list (30%) and detail (70%)
/// - Footer (3 lines): Keybinding help
#[derive(Debug, Clone, Copy)]
pub struct AppLayout {
    /// Header area for title and status
    pub header: Rect,
    /// Left panel for session list (30% of content width)
    pub list_area: Rect,
    /// Right panel for session details (70% of content width)
    pub detail_area: Rect,
    /// Footer area for keybindings
    pub footer: Rect,
}

impl AppLayout {
    /// Creates a new AppLayout by splitting the given area.
    ///
    /// The layout allocates:
    /// - 3 lines for the header
    /// - All remaining space for content (split 30% list / 70% detail)
    /// - 3 lines for the footer
    pub fn new(area: Rect) -> Self {
        // Vertical split: header, content, footer
        let [header, content, footer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Min(10),   // Content (minimum 10 lines)
                Constraint::Length(3), // Footer
            ])
            .areas(area);

        // Horizontal split of content: 30% list, 70% detail
        let [list_area, detail_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30), // List panel
                Constraint::Percentage(70), // Detail panel
            ])
            .areas(content);

        Self {
            header,
            list_area,
            detail_area,
            footer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_layout_creation() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::new(area);

        // Header should be 3 lines at top
        assert_eq!(layout.header.y, 0);
        assert_eq!(layout.header.height, 3);

        // Footer should be 3 lines at bottom
        assert_eq!(layout.footer.height, 3);
        assert_eq!(layout.footer.y + layout.footer.height, 24);

        // List area should be 30% of content width (80 * 0.3 = 24)
        assert_eq!(layout.list_area.width, 24);
        assert_eq!(layout.list_area.y, 3); // Starts after header

        // Detail area should be 70% of content width (80 * 0.7 = 56)
        assert_eq!(layout.detail_area.width, 56);
        assert_eq!(layout.detail_area.y, 3); // Starts after header
    }
}
