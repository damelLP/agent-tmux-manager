//! Session list widget for the ATM TUI.
//!
//! Displays a scrollable list of Claude Code sessions with
//! real-time status updates including context usage, cost, and duration.

use crate::app::{App, AppState};
use crate::ui::theme::{context_color, status_background, status_color, status_icon};
use atm_core::{SessionView, TreeRow, TreeRowKind};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

/// Renders the session list as a tree in the left panel.
///
/// Displays a grouped tree view: Project > Worktree > Agent.
/// Group rows show collapse indicators (▼/▸) and agent counts.
/// Agent rows show status icon, context %, short ID, and model.
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The rectangular area for the session list
/// * `app` - Application state containing tree data
pub fn render_session_list(frame: &mut Frame, area: Rect, app: &App) {
    if app.sessions.is_empty() {
        render_empty_state(frame, area, &app.state);
        return;
    }

    let items: Vec<ListItem> = app
        .tree_rows
        .iter()
        .enumerate()
        .map(|(idx, row)| create_tree_row_item(row, idx == app.selected_index, app.blink_visible))
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

/// Renders a compact session list for narrow sidebars.
///
/// Uses compressed agent rows showing only icon, context %, and name.
/// Group rows (Project, Worktree, Team) reuse the standard group line.
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The rectangular area for the session list
/// * `app` - Application state containing tree data
pub fn render_compact_session_list(frame: &mut Frame, area: Rect, app: &App) {
    if app.sessions.is_empty() {
        render_empty_state(frame, area, &app.state);
        return;
    }

    let inner_width = area.width.saturating_sub(2); // account for borders

    let items: Vec<ListItem> = app
        .tree_rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let is_selected = idx == app.selected_index;
            let indent = "  ".repeat(row.depth as usize);

            let line = match &row.kind {
                TreeRowKind::Project { name, .. } => {
                    create_group_line(&indent, name, row, is_selected)
                }
                TreeRowKind::Worktree { branch, path, .. } => {
                    let label = branch.as_deref().unwrap_or_else(|| {
                        path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
                    });
                    create_group_line(&indent, label, row, is_selected)
                }
                TreeRowKind::Team { name } => {
                    create_group_line(&indent, name, row, is_selected)
                }
                TreeRowKind::Agent { session } => {
                    create_compact_agent_line(
                        &indent, session, is_selected, app.blink_visible, inner_width,
                    )
                }
            };

            let bg_style = match &row.kind {
                TreeRowKind::Agent { session } => {
                    get_row_background_style(session, is_selected)
                }
                _ if is_selected => Style::default().bg(Color::Rgb(30, 30, 40)),
                _ => Style::default(),
            };

            ListItem::new(line).style(bg_style)
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

/// Creates a list item for a single tree row.
fn create_tree_row_item(
    row: &TreeRow,
    is_selected: bool,
    blink_visible: bool,
) -> ListItem<'static> {
    let indent = "  ".repeat(row.depth as usize);

    let line = match &row.kind {
        TreeRowKind::Project { name, .. } => create_group_line(&indent, name, row, is_selected),
        TreeRowKind::Worktree { branch, path, .. } => {
            let label = branch
                .as_deref()
                .unwrap_or_else(|| path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path));
            create_group_line(&indent, label, row, is_selected)
        }
        TreeRowKind::Team { name } => create_group_line(&indent, name, row, is_selected),
        TreeRowKind::Agent { session } => {
            create_agent_line(&indent, session, is_selected, blink_visible)
        }
    };

    let bg_style = match &row.kind {
        TreeRowKind::Agent { session } => get_row_background_style(session, is_selected),
        _ if is_selected => Style::default().bg(Color::Rgb(30, 30, 40)),
        _ => Style::default(),
    };

    ListItem::new(line).style(bg_style)
}

/// Creates a line for a group header (Project, Worktree, Team).
fn create_group_line(indent: &str, label: &str, row: &TreeRow, is_selected: bool) -> Line<'static> {
    let collapse_icon = if !row.has_children {
        " "
    } else if row.is_expanded {
        "▼"
    } else {
        "▸"
    };

    let attention_marker = if row.needs_attention { "!" } else { " " };

    let mut spans = vec![
        // Selection indicator
        Span::styled(
            if is_selected { ">" } else { " " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{indent}{collapse_icon} ")),
        // Group name
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    // Show agent count when collapsed
    if !row.is_expanded && row.agent_count > 0 {
        spans.push(Span::styled(
            format!(" ({})", row.agent_count),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Attention marker
    if row.needs_attention {
        spans.push(Span::styled(
            format!(" {attention_marker}"),
            Style::default().fg(Color::Yellow),
        ));
    }

    Line::from(spans)
}

/// Creates a line for an agent (session) row.
fn create_agent_line(
    indent: &str,
    session: &SessionView,
    is_selected: bool,
    blink_visible: bool,
) -> Line<'static> {
    let context_pct = session.context_percentage;
    let ctx_color = context_color(context_pct, session.context_critical);
    let icon = status_icon(session.status, blink_visible);
    let icon_color = status_color(session.status);

    let spans = vec![
        // Selection indicator
        Span::styled(
            if is_selected { ">" } else { " " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(indent.to_string()),
        // Status icon
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

    Line::from(spans)
}

/// Creates a compact line for an agent row in narrow sidebars.
///
/// Format: `{selector}{indent}{icon} {ctx%} {name}`
/// Name is resolved from worktree branch (preferred) or short ID (fallback),
/// then adaptively truncated to fit `available_width`.
fn create_compact_agent_line(
    indent: &str,
    session: &SessionView,
    is_selected: bool,
    blink_visible: bool,
    available_width: u16,
) -> Line<'static> {
    let icon = status_icon(session.status, blink_visible);
    let icon_color = status_color(session.status);
    let ctx_color = context_color(session.context_percentage, session.context_critical);

    let name = session.worktree_branch.as_deref()
        .unwrap_or(&session.id_short);

    let overhead = 9u16.saturating_add(indent.len() as u16);
    let name_width = available_width.saturating_sub(overhead) as usize;
    let truncated_name = truncate_string(name, name_width.max(1));

    let spans = vec![
        Span::styled(
            if is_selected { ">" } else { " " },
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(indent.to_string()),
        Span::styled(
            format!("{icon} "),
            Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>4.0}%", session.context_percentage),
            Style::default().fg(ctx_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(truncated_name, Style::default().fg(Color::White)),
    ];

    Line::from(spans)
}

/// Returns the background style for a session row.
fn get_row_background_style(session: &SessionView, is_selected: bool) -> Style {
    let bg_color = status_background(session.status).or(if session.context_critical {
        Some(Color::Rgb(40, 0, 0))
    } else if is_selected {
        Some(Color::Rgb(30, 30, 40))
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
    use atm_core::SessionStatus;

    #[test]
    fn test_status_icon_via_theme() {
        assert_eq!(status_icon(SessionStatus::Working, true), ">");
    }

    #[test]
    fn test_status_attention_needed_blinks() {
        assert_eq!(status_icon(SessionStatus::AttentionNeeded, true), "!");
        assert_eq!(status_icon(SessionStatus::AttentionNeeded, false), " ");
    }

    #[test]
    fn test_status_idle_no_blink() {
        assert_eq!(status_icon(SessionStatus::Idle, true), "-");
        assert_eq!(status_icon(SessionStatus::Idle, false), "-");
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
        assert_eq!(truncate_string("hello🔥world", 8), "hello...");
        assert_eq!(truncate_string("🔥🔥🔥🔥🔥", 5), "🔥🔥🔥🔥🔥");
        assert_eq!(truncate_string("🔥🔥🔥🔥🔥", 4), "🔥...");
    }
}
