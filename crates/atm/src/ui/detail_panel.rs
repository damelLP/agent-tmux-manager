//! Session detail panel widget for the ATM TUI.
//!
//! Displays detailed information about a selected session
//! in the right panel of the split layout.

use crate::beads::BeadsTask;
use crate::ui::theme::{context_color, status_color};
use atm_core::{format_age, SessionStatus, SessionView};
use chrono::{DateTime, Utc};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
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
/// * `now` - Time of this frame, used for the relative "Activity" age
pub fn render_detail_panel_inline(
    frame: &mut Frame,
    area: Rect,
    session: Option<&SessionView>,
    captured_output: &[String],
    now: DateTime<Utc>,
) {
    match session {
        Some(session) => {
            // Build the detail content (reuse existing logic)
            let lines = build_detail_lines_inline(session, now);

            // Determine border color based on session state
            let border_color = if session.context_critical {
                Color::Red
            } else if session.context_warning || session.needs_attention {
                Color::Yellow
            } else {
                Color::Cyan
            };

            if captured_output.is_empty() {
                // No capture data — full area for metadata (existing behavior)
                let block = Block::default()
                    .title(" Details ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color));

                let paragraph = Paragraph::new(lines).block(block);
                frame.render_widget(paragraph, area);
            } else {
                // Split: top 40% metadata, bottom 60% terminal capture
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .split(area);

                let meta_area = chunks.first().copied().unwrap_or(area);
                let capture_area = chunks.get(1).copied().unwrap_or(area);

                // Render metadata section
                let meta_block = Block::default()
                    .title(" Details ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color));

                let meta_paragraph = Paragraph::new(lines).block(meta_block);
                frame.render_widget(meta_paragraph, meta_area);

                // Render terminal capture section
                let capture_block = Block::default()
                    .title(" Terminal ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray));

                // Show last N lines that fit (auto-scroll to bottom)
                let inner_height = capture_area.height.saturating_sub(2) as usize;
                let start = captured_output.len().saturating_sub(inner_height);
                let visible_lines: Vec<Line<'_>> = captured_output
                    .iter()
                    .skip(start)
                    .map(|l| Line::from(Span::raw(l.as_str())))
                    .collect();

                let capture_paragraph = Paragraph::new(visible_lines).block(capture_block);
                frame.render_widget(capture_paragraph, capture_area);
            }
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
fn build_detail_lines_inline(session: &SessionView, now: DateTime<Utc>) -> Vec<Line<'static>> {
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
            Span::styled("  Harness: ", label_style),
            Span::styled(
                if session.harness.is_empty() {
                    "claude".to_string()
                } else {
                    session.harness.clone()
                },
                value_style,
            ),
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
            Span::styled(last_activity_age(session, now), value_style),
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

/// Age of the session's last activity at `now`.
///
/// Falls back to the daemon's preformatted string when `last_activity`
/// is not RFC 3339.
fn last_activity_age(session: &SessionView, now: DateTime<Utc>) -> String {
    DateTime::parse_from_rfc3339(&session.last_activity)
        .map(|at| format_age(at.with_timezone(&Utc), now))
        .unwrap_or_else(|_| session.last_activity_display.clone())
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

/// Renders the compact preview pane: beads task + first prompt summary.
///
/// Content (word-wrapped to fill available space):
/// - Beads in-progress task title (cyan, bold)
/// - First user prompt
/// - Fallback: status info if nothing else available
///
/// `beads_task` is the cached task for the session's project directory;
/// rendering never reads `.beads/` itself.
pub fn render_compact_preview(
    frame: &mut Frame,
    area: Rect,
    session: Option<&SessionView>,
    _captured_output: &[String],
    beads_task: Option<&BeadsTask>,
) {
    let block = Block::default()
        .title(" Summary ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let lines: Vec<Line<'_>> = match session {
        Some(s) => {
            let mut result: Vec<Line<'_>> = Vec::new();

            if let Some(task) = beads_task {
                result.push(Line::from(Span::styled(
                    task.title.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                if let Some(ref desc) = task.description {
                    for line in desc.lines() {
                        result.push(Line::from(Span::styled(
                            line.to_string(),
                            Style::default().fg(Color::White),
                        )));
                    }
                }
            }

            // Fallback: status info
            if result.is_empty() {
                let status_line = match &s.activity_detail {
                    Some(detail) => format!("{} ({})", s.status_label, detail),
                    None => s.status_label.clone(),
                };
                result.push(Line::from(Span::styled(
                    status_line,
                    Style::default().fg(status_color(s.status)),
                )));
            }

            result
        }
        None => vec![Line::from(Span::styled(
            "No session selected",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(paragraph, area);
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

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn detail_text(session: &SessionView, now: DateTime<Utc>) -> String {
        build_detail_lines_inline(session, now)
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect()
    }

    #[test]
    fn test_activity_age_follows_frame_time() {
        // The daemon formatted "0s ago" when it published this view; an
        // idle session sends nothing newer, so the panel must age it.
        let session = SessionView {
            last_activity: "2026-01-15T10:05:00+00:00".to_string(),
            last_activity_display: "0s ago".to_string(),
            ..Default::default()
        };

        assert!(detail_text(&session, at("2026-01-15T10:05:00Z")).contains("Activity: 0s ago"));
        assert!(detail_text(&session, at("2026-01-15T10:05:36Z")).contains("Activity: 36s ago"));
    }

    #[test]
    fn test_activity_age_falls_back_to_daemon_text() {
        for last_activity in ["", "not a timestamp"] {
            let session = SessionView {
                last_activity: last_activity.to_string(),
                last_activity_display: "10s ago".to_string(),
                ..Default::default()
            };

            let text = detail_text(&session, at("2026-01-15T10:05:36Z"));
            assert!(
                text.contains("Activity: 10s ago"),
                "{last_activity:?}: {text}"
            );
        }
    }
}
