//! Help popup overlay showing all keybindings.
//!
//! Renders a centered popup on top of the existing layout, listing
//! available keyboard shortcuts grouped by category. Content is derived
//! from [`KEYBINDING_HINTS`] in `keybinding.rs`.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::keybinding::{HintCategory, KEYBINDING_HINTS};

use super::layout::centered_popup;

/// Renders the help popup overlay.
///
/// Clears the background behind the popup and renders a bordered
/// paragraph listing all keybindings grouped by category.
/// Filters out tmux-only entries when not running inside tmux.
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The full terminal area (popup will be centered within it)
pub fn render_help_popup(frame: &mut Frame, area: Rect) {
    let popup_area = centered_popup(60, 70, area);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    let in_tmux = crate::tmux::is_in_tmux();
    let lines = build_help_lines(in_tmux);

    let popup = Paragraph::new(lines).block(
        Block::default()
            .title(" Help ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(popup, popup_area);
}

/// Builds the styled content lines for the help popup.
///
/// Groups keybindings by category with headings, and filters out
/// tmux-only entries when `in_tmux` is false.
fn build_help_lines(in_tmux: bool) -> Vec<Line<'static>> {
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let heading_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let mut lines = vec![Line::from("")];
    let mut current_category = None;

    for entry in KEYBINDING_HINTS {
        // Skip tmux-only entries when not in tmux
        if entry.tmux_only && !in_tmux {
            continue;
        }

        // Insert category heading when category changes
        if current_category != Some(entry.category) {
            if current_category.is_some() {
                lines.push(Line::from(""));
            }
            let heading = match entry.category {
                HintCategory::Navigation => "  Navigation",
                HintCategory::Actions => "  Actions",
            };
            lines.push(Line::from(Span::styled(heading, heading_style)));
            lines.push(Line::from(""));
            current_category = Some(entry.category);
        }

        lines.push(Line::from(vec![
            Span::styled(format!("    {:<11} ", entry.help_key), key_style),
            Span::raw(entry.help_desc),
        ]));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    /// Extract the raw text content from a Line (stripping styles).
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Helper: build lines and return as plain text strings.
    fn help_texts(in_tmux: bool) -> Vec<String> {
        build_help_lines(in_tmux).iter().map(line_text).collect()
    }

    #[test]
    fn test_category_structure() {
        let texts = help_texts(true);

        // Starts with a blank line
        assert_eq!(texts[0], "");

        // Both category headings present
        assert!(texts.iter().any(|t| t.contains("Navigation")));
        assert!(texts.iter().any(|t| t.contains("Actions")));

        // Blank line separates categories
        let actions_idx = texts
            .iter()
            .position(|t| t.contains("Actions"))
            .expect("Actions heading should exist");
        assert_eq!(texts[actions_idx - 1], "", "blank line before Actions");
    }

    #[test]
    fn test_all_hints_present_when_in_tmux() {
        let texts = help_texts(true);
        for entry in KEYBINDING_HINTS {
            assert!(
                texts.iter().any(|t| t.contains(entry.help_desc)),
                "Missing entry: {:?}",
                entry.help_desc
            );
        }
    }

    #[test]
    fn test_tmux_only_entries_filtered_when_not_in_tmux() {
        let with = help_texts(true);
        let without = help_texts(false);

        // Fewer lines when tmux-only entries are filtered
        assert!(without.len() < with.len());

        // tmux-only entries absent
        for entry in KEYBINDING_HINTS.iter().filter(|e| e.tmux_only) {
            assert!(
                !without.iter().any(|t| t.contains(entry.help_desc)),
                "tmux-only entry should be filtered: {:?}",
                entry.help_desc
            );
        }
    }

    #[test]
    fn test_key_column_width_consistent() {
        let lines = build_help_lines(true);

        // Entry lines have exactly 2 spans (styled key + raw description).
        // Key column: "    {:<11} " = 16 chars.
        // Use chars().count() — some keys contain multi-byte Unicode (↓, ↑).
        for line in &lines {
            if line.spans.len() == 2 {
                let char_count = line.spans[0].content.chars().count();
                assert_eq!(
                    char_count, 16,
                    "Key column should be 16 chars, got {char_count} for {:?}",
                    line.spans[0].content
                );
            }
        }
    }

    #[test]
    fn test_render_smoke_various_sizes() {
        for (w, h) in [(80, 24), (40, 12), (10, 5), (200, 50)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render_help_popup(frame, frame.area()))
                .unwrap();
        }
    }
}
