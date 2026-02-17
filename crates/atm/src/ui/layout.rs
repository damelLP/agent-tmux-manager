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

/// Returns a centered popup `Rect` within the given `area`.
///
/// The popup occupies `percent_x`% of the width and `percent_y`% of the height,
/// centered in `area`. Percentages are clamped to 100.
///
/// Uses clamping and saturating arithmetic so that the computed popup rectangle
/// remains within the given area and within the valid `u16` range, even for
/// extreme or degenerate inputs.
#[must_use]
pub fn centered_popup(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let px = percent_x.min(100);
    let py = percent_y.min(100);

    let popup_width = (area.width as u32 * px as u32 / 100) as u16;
    let popup_height = (area.height as u32 * py as u32 / 100) as u16;

    let x = area
        .x
        .saturating_add(area.width.saturating_sub(popup_width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(popup_height) / 2);

    Rect::new(x, y, popup_width, popup_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- centered_popup tests ------------------------------------------------

    #[test]
    fn test_centered_popup_50x50() {
        let area = Rect::new(0, 0, 80, 24);
        let popup = centered_popup(50, 50, area);
        assert_eq!(popup.width, 40);
        assert_eq!(popup.height, 12);
        assert_eq!(popup.x, 20);
        assert_eq!(popup.y, 6);
    }

    #[test]
    fn test_centered_popup_100x100() {
        let area = Rect::new(0, 0, 80, 24);
        let popup = centered_popup(100, 100, area);
        assert_eq!(popup.width, 80);
        assert_eq!(popup.height, 24);
        assert_eq!(popup.x, 0);
        assert_eq!(popup.y, 0);
    }

    #[test]
    fn test_centered_popup_zero_area() {
        let area = Rect::new(0, 0, 0, 0);
        let popup = centered_popup(50, 50, area);
        assert_eq!(popup.width, 0);
        assert_eq!(popup.height, 0);
    }

    #[test]
    fn test_centered_popup_over_100_clamps() {
        let area = Rect::new(0, 0, 80, 24);
        let popup = centered_popup(200, 150, area);
        // Should behave like 100x100
        assert_eq!(popup.width, 80);
        assert_eq!(popup.height, 24);
        assert_eq!(popup.x, 0);
        assert_eq!(popup.y, 0);
    }

    // -- AppLayout tests -----------------------------------------------------

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
