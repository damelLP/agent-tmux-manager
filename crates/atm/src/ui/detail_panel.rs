//! Session detail panel widget for the ATM TUI.
//!
//! Displays detailed information about a selected session
//! in the right panel of the split layout.

use crate::ui::theme::{context_color, status_color};
use atm_core::{SessionStatus, SessionView};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Renders the session detail panel inline (for split layout).
///
/// Unlike `render_detail_panel`, this renders directly into the provided area
/// without centering or clearing background. Used for the always-visible
/// split layout.
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The rectangular area to fill
/// * `session` - The session to display (or None for empty state)
pub fn render_detail_panel_inline(frame: &mut Frame, area: Rect, session: Option<&SessionView>) {
    match session {
        Some(session) => {
            // Build the detail content (reuse existing logic)
            let lines = build_detail_lines_inline(session);

            // Determine border color based on session state
            let border_color = if session.context_critical {
                Color::Red
            } else if session.context_warning || session.needs_attention {
                Color::Yellow
            } else {
                Color::Cyan
            };

            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color));

            let paragraph = Paragraph::new(lines).block(block);
            frame.render_widget(paragraph, area);
        }
        None => {
            // Empty state - no session selected
            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));

            let paragraph = Paragraph::new("").block(block);
            frame.render_widget(paragraph, area);
        }
    }
}

/// Builds detail lines for inline panel (condensed format).
fn build_detail_lines_inline(session: &SessionView) -> Vec<Line<'static>> {
    let label_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(Color::White);

    let ctx_color = context_color(session.context_percentage, session.context_critical);

    // Status line with activity detail
    let status_display = match session.activity_detail.as_ref() {
        Some(detail) => format!("{} ({})", session.status_label, detail),
        None => session.status_label.clone(),
    };

    let status_style = Style::default()
        .fg(status_color(session.status))
        .add_modifier(
            if matches!(
                session.status,
                SessionStatus::Working | SessionStatus::AttentionNeeded
            ) {
                Modifier::BOLD
            } else {
                Modifier::empty()
            },
        );

    let mut lines = vec![
        Line::from(""),
        // Status - prominent at top
        Line::from(vec![
            Span::styled("  Status: ", label_style),
            Span::styled(status_display, status_style),
        ]),
        Line::from(""),
        // Identity - one line
        Line::from(vec![
            Span::styled("  ID: ", label_style),
            Span::styled(session.id_short.clone(), value_style),
            Span::styled("  Agent: ", label_style),
            Span::styled(session.agent_type.clone(), value_style),
            Span::styled("  Model: ", label_style),
            Span::styled(session.model.clone(), value_style),
        ]),
        Line::from(""),
        // Context bar
        Line::from(vec![
            Span::styled("  Context ", label_style),
            Span::styled(
                build_progress_bar(session.context_percentage, 20),
                Style::default().fg(ctx_color),
            ),
            Span::styled(
                format!(" ({})", session.context_display),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        // Duration and activity
        Line::from(vec![
            Span::styled("  Duration: ", label_style),
            Span::styled(session.duration_display.clone(), value_style),
            Span::styled("    Activity: ", label_style),
            Span::styled(session.last_activity_display.clone(), value_style),
        ]),
        // Lines changed
        Line::from(vec![
            Span::styled("  Lines:    ", label_style),
            Span::styled(session.lines_display.clone(), value_style),
        ]),
        Line::from(""),
    ];

    // Working directory
    if let Some(ref dir) = session.working_directory {
        lines.push(Line::from(vec![
            Span::styled("  Dir: ", label_style),
            Span::styled(dir.clone(), Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(""));
    }

    // Warnings
    if session.needs_attention {
        lines.push(Line::from(vec![Span::styled(
            "  ! Waiting for input",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
    }

    lines
}

/// Builds an ASCII progress bar for context usage.
///
/// # Arguments
/// * `percentage` - The fill percentage (0-100)
/// * `width` - The width of the bar in characters
///
/// # Returns
/// A string representing the progress bar
fn build_progress_bar(percentage: f64, width: usize) -> String {
    // Handle NaN, infinity, and negative values safely
    let safe_percentage = if percentage.is_nan() || percentage.is_infinite() || percentage < 0.0 {
        0.0
    } else {
        percentage
    };

    let filled = ((safe_percentage / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width); // Clamp to max width
    let empty = width.saturating_sub(filled);

    format!(
        "[{}{}] {:.0}%",
        "=".repeat(filled),
        " ".repeat(empty),
        safe_percentage
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_bar_empty() {
        let bar = build_progress_bar(0.0, 10);
        assert_eq!(bar, "[          ] 0%");
    }

    #[test]
    fn test_progress_bar_half() {
        let bar = build_progress_bar(50.0, 10);
        assert_eq!(bar, "[=====     ] 50%");
    }

    #[test]
    fn test_progress_bar_full() {
        let bar = build_progress_bar(100.0, 10);
        assert_eq!(bar, "[==========] 100%");
    }

    #[test]
    fn test_progress_bar_over_100() {
        let bar = build_progress_bar(120.0, 10);
        // Should be clamped to 100%
        assert_eq!(bar, "[==========] 120%");
    }

    #[test]
    fn test_progress_bar_nan() {
        let bar = build_progress_bar(f64::NAN, 10);
        // NaN should be treated as 0%
        assert_eq!(bar, "[          ] 0%");
    }

    #[test]
    fn test_progress_bar_infinity() {
        let bar = build_progress_bar(f64::INFINITY, 10);
        // Infinity should be treated as 0%
        assert_eq!(bar, "[          ] 0%");
    }

    #[test]
    fn test_progress_bar_negative() {
        let bar = build_progress_bar(-50.0, 10);
        // Negative should be treated as 0%
        assert_eq!(bar, "[          ] 0%");
    }

    // Note: context_color tests are now in ui::theme module
}
