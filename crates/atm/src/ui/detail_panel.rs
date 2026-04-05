//! Session detail panel widget for the ATM TUI.
//!
//! Displays detailed information about a selected session
//! in the right panel of the split layout.

use crate::ui::theme::{context_color, status_color};
use atm_core::{SessionStatus, SessionView};
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
pub fn render_detail_panel_inline(
    frame: &mut Frame,
    area: Rect,
    session: Option<&SessionView>,
    captured_output: &[String],
) {
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

/// Renders the compact preview pane: beads task + prompt summary, then terminal capture.
///
/// Content:
/// - Beads in-progress task title (if found at project root)
/// - First user prompt (word-wrapped by ratatui)
/// - Terminal capture below (auto-scrolls to bottom)
pub fn render_compact_preview(
    frame: &mut Frame,
    area: Rect,
    session: Option<&SessionView>,
    captured_output: &[String],
) {
    // Build summary text (beads task + first prompt)
    let summary_text: Vec<Line<'_>> = match session {
        Some(s) => {
            let mut result: Vec<Line<'_>> = Vec::new();

            // Beads task: try project_root first (where .beads/ lives), then worktree_path
            let beads_title = s.project_root.as_deref()
                .or(s.worktree_path.as_deref())
                .and_then(|dir| {
                    let tasks = atm_core::beads::find_in_progress_tasks(dir);
                    tasks.into_iter().next().map(|t| t.title)
                });

            if let Some(title) = beads_title {
                result.push(Line::from(Span::styled(
                    title,
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )));
            }

            if let Some(ref prompt) = s.first_prompt {
                result.push(Line::from(Span::styled(
                    prompt.clone(),
                    Style::default().fg(Color::White),
                )));
            }

            result
        }
        None => Vec::new(),
    };

    let has_summary = !summary_text.is_empty();

    if !has_summary {
        // No summary — give all space to terminal capture
        render_terminal_capture(frame, area, captured_output);
        return;
    }

    // Split: summary gets ~40% (min 4 lines), terminal gets rest
    let [summary_area, capture_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Min(3),
        ])
        .areas(area);

    let block = Block::default()
        .title(" Summary ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let paragraph = Paragraph::new(summary_text)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(paragraph, summary_area);

    render_terminal_capture(frame, capture_area, captured_output);
}

/// Renders the terminal capture section (auto-scrolls to bottom).
pub fn render_terminal_capture(
    frame: &mut Frame,
    area: Rect,
    captured_output: &[String],
) {
    let block = Block::default()
        .title(" Terminal ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    if captured_output.is_empty() {
        let paragraph = Paragraph::new("").block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    // Strip ANSI escape codes from captured output for clean rendering.
    // Terminal capture may contain color codes, cursor movement, etc.
    let stripped: Vec<String> = captured_output
        .iter()
        .map(|l| strip_ansi_codes(l))
        .collect();

    let inner_height = area.height.saturating_sub(2) as usize;

    // Take only the last N lines to keep rendering fast, then let ratatui wrap
    let tail_start = stripped.len().saturating_sub(inner_height * 3); // 3x headroom for wrapping
    let visible_lines: Vec<Line<'_>> = stripped[tail_start..]
        .iter()
        .map(|l| Line::from(Span::raw(l.as_str())))
        .collect();

    // Scroll to bottom: use a large value, ratatui clamps it
    let scroll = visible_lines.len().saturating_sub(1) as u16;

    let paragraph = Paragraph::new(visible_lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

/// Strips ANSI escape sequences from a string.
fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC [ ... <final byte> sequences (CSI)
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Consume parameter bytes (0x30-0x3F) and intermediate bytes (0x20-0x2F)
                // until final byte (0x40-0x7E)
                loop {
                    match chars.peek() {
                        Some(&c) if ('\x40'..='\x7e').contains(&c) => {
                            chars.next(); // consume final byte
                            break;
                        }
                        Some(_) => { chars.next(); }
                        None => break,
                    }
                }
            }
            // Skip ESC ] ... ST sequences (OSC) — terminated by BEL or ESC \
            else if chars.peek() == Some(&']') {
                chars.next();
                loop {
                    match chars.next() {
                        Some('\x07') => break, // BEL
                        Some('\x1b') if chars.peek() == Some(&'\\') => {
                            chars.next();
                            break;
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
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
