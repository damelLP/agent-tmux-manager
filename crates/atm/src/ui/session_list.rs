//! Session list widget for the ATM TUI.
//!
//! Displays a scrollable list of Claude Code sessions with
//! real-time status updates including context usage, cost, and duration.

use crate::app::{App, AppState};
use crate::ui::theme::{context_color, status_background, status_color, status_icon};
use atm_core::SessionView;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

/// Renders the session list in the left panel.
///
/// Shows a condensed view of active sessions with:
/// - Display state icon (>, ~, !, z) - shows working/compacting/needs input/stale
/// - Context usage percentage (color-coded)
/// - Short session ID
/// - Model name
///
/// Row backgrounds indicate status:
/// - Yellow tint: needs input (waiting for user)
/// - Red tint: critical context usage
///
/// When no sessions are available, displays a context-sensitive
/// empty state message.
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The rectangular area for the session list
/// * `app` - Application state containing session data
pub fn render_session_list(frame: &mut Frame, area: Rect, app: &App) {
    if app.sessions.is_empty() {
        render_empty_state(frame, area, &app.state);
        return;
    }

    let sessions = app.sessions_sorted();

    let items: Vec<ListItem> = sessions
        .iter()
        .enumerate()
        .map(|(idx, session)| {
            create_session_item(session, idx == app.selected_index, app.blink_visible)
        })
        .collect();

    let title = format!(" Sessions ({}) ", app.session_count());

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(list, area);
}

/// Creates a list item for a single session (condensed format).
///
/// The format is optimized for the narrow 30% list panel:
/// `> [icon] [percentage] [id] [model]`
///
/// # Arguments
/// * `session` - The session view to render
/// * `is_selected` - Whether this session is currently selected
/// * `blink_visible` - Whether blinking icons should be visible this frame
///
/// # Returns
/// A styled ListItem for the session with appropriate background color
fn create_session_item(
    session: &SessionView,
    is_selected: bool,
    blink_visible: bool,
) -> ListItem<'static> {
    let context_pct = session.context_percentage;
    let ctx_color = context_color(context_pct, session.context_critical);

    // Get status icon and color from theme
    let icon = status_icon(session.status, blink_visible);
    let icon_color = status_color(session.status);

    // Build the condensed line
    let spans = vec![
        // Selection indicator
        Span::styled(
            if is_selected { ">" } else { " " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        // Status icon (blinking for attention states)
        Span::styled(
            format!("{icon} "),
            Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
        ),
        // Context percentage
        Span::styled(
            format!("{context_pct:>4.0}%"),
            Style::default().fg(ctx_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        // Short ID
        Span::styled(
            session.id_short.clone(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(" "),
        // Model (truncated)
        Span::styled(
            truncate_string(&session.model, 8),
            Style::default().fg(Color::White),
        ),
    ];

    // Determine row background color based on status
    let bg_style = get_row_background_style(session, is_selected);

    ListItem::new(Line::from(spans)).style(bg_style)
}

/// Returns the background style for a session row.
fn get_row_background_style(session: &SessionView, is_selected: bool) -> Style {
    // Priority: status background > critical context > selection
    let bg_color = status_background(session.status).or(if session.context_critical {
        Some(Color::Rgb(40, 0, 0)) // Subtle red tint
    } else if is_selected {
        Some(Color::Rgb(30, 30, 40)) // Subtle selection highlight
    } else {
        None
    });

    match bg_color {
        Some(color) => Style::default().bg(color),
        None => Style::default(),
    }
}

/// Truncates a string to the specified maximum display width.
///
/// If truncated, appends "..." to indicate truncation.
/// Handles UTF-8 multi-byte characters safely by counting chars, not bytes.
///
/// # Arguments
/// * `s` - The string to truncate
/// * `max_len` - Maximum character count including any ellipsis
///
/// # Returns
/// The truncated string
fn truncate_string(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        s.chars().take(max_len).collect()
    } else {
        let truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

/// Renders the empty state when no sessions are available.
///
/// Shows different messages based on connection state:
/// - Connected: Instructions to start a session
/// - Connecting: Loading indicator
/// - Disconnected: Troubleshooting hints
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The rectangular area for the empty state
/// * `state` - Current connection state
fn render_empty_state(frame: &mut Frame, area: Rect, state: &AppState) {
    let (title, lines) = match state {
        AppState::Connected => (
            " No Sessions ",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No active Claude Code sessions detected",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from("To get started:"),
                Line::from(""),
                Line::from(Span::styled(
                    "  1. Open a terminal",
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    "  2. Run: claude",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    "  3. Session will appear here automatically",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Tip: Make sure Claude Code is configured with atm integration",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )),
            ],
        ),
        AppState::Connecting => (
            " Connecting ",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Connecting to ATM daemon...",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from("This usually takes 1-2 seconds."),
                Line::from(""),
                Line::from(Span::styled(
                    "If this persists, check: atmd status",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )),
            ],
        ),
        AppState::Disconnected { retry_count, .. } => (
            " Disconnected ",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Lost connection to daemon",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(format!("Retry attempt: {retry_count}")),
                Line::from(""),
                Line::from("Troubleshooting:"),
                Line::from(""),
                Line::from(Span::styled(
                    "  1. Check daemon: atmd status",
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    "  2. View logs: tail ~/.local/state/atm/atm.log",
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    "  3. Restart daemon: atmd restart",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Press 'q' to quit",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
        ),
    };

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(match state {
                AppState::Connected => Style::default().fg(Color::Yellow),
                AppState::Connecting => Style::default().fg(Color::Yellow),
                AppState::Disconnected { .. } => Style::default().fg(Color::Red),
            }),
    );

    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::{SessionId, SessionStatus};

    fn test_session() -> SessionView {
        SessionView {
            id: SessionId::new("test-session-id"),
            id_short: "test-ses".to_string(),
            agent_type: "general".to_string(),
            model: "Opus 4.5".to_string(),
            status: SessionStatus::Working,
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
            working_directory: None,

            needs_attention: false,
            last_activity_display: "10s ago".to_string(),
            age_display: "5m ago".to_string(),
            started_at: "2024-01-15T10:00:00Z".to_string(),
            last_activity: "2024-01-15T10:05:00Z".to_string(),
            tmux_pane: None,
        }
    }

    #[test]
    fn test_status_icon_via_theme() {
        // Icon logic is now in theme module, just verify integration
        let session = test_session();
        assert_eq!(status_icon(session.status, true), ">");
    }

    #[test]
    fn test_status_attention_needed_blinks() {
        let mut session = test_session();
        session.status = SessionStatus::AttentionNeeded;
        assert_eq!(status_icon(session.status, true), "!");
        assert_eq!(status_icon(session.status, false), " ");
    }

    #[test]
    fn test_status_idle_no_blink() {
        let mut session = test_session();
        session.status = SessionStatus::Idle;
        // Idle does NOT blink - it's chill
        assert_eq!(status_icon(session.status, true), "-");
        assert_eq!(status_icon(session.status, false), "-");
    }

    #[test]
    fn test_truncate_string_short() {
        assert_eq!(truncate_string("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_string_exact() {
        assert_eq!(truncate_string("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_string_long() {
        assert_eq!(truncate_string("hello world", 8), "hello...");
    }

    #[test]
    fn test_truncate_string_very_short_max() {
        assert_eq!(truncate_string("hello", 2), "he");
    }

    #[test]
    fn test_truncate_string_utf8_multibyte() {
        // Test with UTF-8 multi-byte characters (emoji is 1 char but multiple bytes)
        assert_eq!(truncate_string("hello🔥world", 8), "hello...");
        // 5 chars fits exactly
        assert_eq!(truncate_string("🔥🔥🔥🔥🔥", 5), "🔥🔥🔥🔥🔥");
        // Truncation with ellipsis
        assert_eq!(truncate_string("🔥🔥🔥🔥🔥", 4), "🔥...");
    }
}
