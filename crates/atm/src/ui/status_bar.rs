//! Header and footer status bar widgets for the ATM TUI.
//!
//! The status bar provides:
//! - Header: Application title and connection status indicator
//! - Footer: Keybinding hints for user navigation

use crate::app::{App, AppState};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Renders the header bar with title, connection status, and summary statistics.
///
/// The header displays:
/// - Application name and description
/// - Current connection status with color-coded indicator
/// - Session count and summary stats when connected
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The rectangular area for the header
/// * `app` - Application state containing connection info
pub fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let (status_text, status_style) = get_status_display(&app.state);

    let session_count = app.session_count();

    // Build summary stats when we have sessions
    let stats_display = if session_count > 0 {
        let total_cost = app.total_cost();
        let avg_context = app.average_context();
        let attention = app.attention_count();
        let working = app.working_count();

        // Format cost
        let cost_str = if total_cost >= 1.0 {
            format!("${total_cost:.2}")
        } else {
            format!("${total_cost:.3}")
        };

        // Build stats string
        let mut stats = format!(
            " | {} session{} | {} | avg {}%",
            session_count,
            if session_count == 1 { "" } else { "s" },
            cost_str,
            avg_context as u32
        );

        // Add working/attention counts if non-zero
        if working > 0 {
            stats.push_str(&format!(" | {working} working"));
        }
        if attention > 0 {
            stats.push_str(&format!(" | {attention} need input"));
        }

        stats
    } else {
        String::new()
    };

    let header_line = Line::from(vec![
        Span::styled(
            "ATM",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" - Claude Code Monitor | "),
        Span::styled(status_text, status_style),
        Span::styled(stats_display, Style::default().fg(Color::DarkGray)),
    ]);

    let border_style = match app.state {
        AppState::Connected => Style::default().fg(Color::Green),
        AppState::Connecting => Style::default().fg(Color::Yellow),
        AppState::Disconnected { .. } => Style::default().fg(Color::Red),
    };

    let header = Paragraph::new(header_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style),
    );

    frame.render_widget(header, area);
}

/// Renders the footer bar with keybinding hints.
///
/// The footer displays available keyboard shortcuts with
/// highlighted key indicators. Shows different hints based on
/// whether we're in tmux (for jump functionality).
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The rectangular area for the footer
/// * `app` - Application state (used for pick_mode indicator)
pub fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let sep_style = Style::default().fg(Color::DarkGray);

    // Check if we're in tmux for jump hint (use centralized function)
    let in_tmux = crate::tmux::is_in_tmux();

    // Build footer hints from keybinding metadata
    let mut hints = Vec::new();
    let mut last_category = None;

    for entry in crate::keybinding::KEYBINDING_HINTS {
        // Skip entries not shown in footer
        if entry.footer_key.is_empty() {
            continue;
        }

        // Skip tmux-only entries when not in tmux
        if entry.tmux_only && !in_tmux {
            continue;
        }

        // Add separator between categories
        if let Some(prev) = last_category {
            if prev != entry.category {
                hints.push(Span::styled("  |  ", sep_style));
            }
        }
        last_category = Some(entry.category);

        // Add leading space before first entry
        if hints.is_empty() {
            hints.push(Span::styled(format!(" {}", entry.footer_key), key_style));
        } else {
            hints.push(Span::styled(entry.footer_key, key_style));
        }
        hints.push(Span::raw(format!(" {}  ", entry.footer_desc)));
    }

    // Show pick mode indicator
    if app.pick_mode {
        hints.push(Span::styled("  |  ", sep_style));
        hints.push(Span::styled(
            "[pick mode]",
            Style::default().fg(Color::Yellow),
        ));
    }

    let footer_line = Line::from(hints);

    let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));

    frame.render_widget(footer, area);
}

/// Renders a minimal single-line footer for compact mode.
pub fn render_compact_footer(frame: &mut Frame, area: Rect, app: &App) {
    let text = if app.pick_mode {
        "? help [pick]"
    } else {
        "? help"
    };
    let paragraph = Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(paragraph, area);
}

/// Returns the display text and style for the given connection state.
///
/// # Arguments
/// * `state` - The current application connection state
///
/// # Returns
/// A tuple of (display_text, style) for the status indicator
fn get_status_display(state: &AppState) -> (&'static str, Style) {
    match state {
        AppState::Connected => (
            "Connected",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        AppState::Connecting => (
            "Connecting...",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        AppState::Disconnected { retry_count, .. } => {
            // Show retry count in status for visibility
            if *retry_count > 3 {
                (
                    "Disconnected (retrying...)",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    "Disconnected",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_display_connected() {
        let (text, style) = get_status_display(&AppState::Connected);
        assert_eq!(text, "Connected");
        assert_eq!(style.fg, Some(Color::Green));
    }

    #[test]
    fn test_status_display_connecting() {
        let (text, style) = get_status_display(&AppState::Connecting);
        assert_eq!(text, "Connecting...");
        assert_eq!(style.fg, Some(Color::Yellow));
    }

    #[test]
    fn test_status_display_disconnected() {
        let state = AppState::Disconnected {
            since: chrono::Utc::now(),
            retry_count: 1,
        };
        let (text, style) = get_status_display(&state);
        assert_eq!(text, "Disconnected");
        assert_eq!(style.fg, Some(Color::Red));
    }

    #[test]
    fn test_status_display_disconnected_many_retries() {
        let state = AppState::Disconnected {
            since: chrono::Utc::now(),
            retry_count: 5,
        };
        let (text, _) = get_status_display(&state);
        assert_eq!(text, "Disconnected (retrying...)");
    }
}
