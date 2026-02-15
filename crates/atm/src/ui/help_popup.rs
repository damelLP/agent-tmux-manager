//! Help popup overlay showing all keybindings.
//!
//! Renders a centered popup on top of the existing layout, listing
//! available keyboard shortcuts grouped by category.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::layout::centered_popup;

/// Renders the help popup overlay.
///
/// Clears the background behind the popup and renders a bordered
/// paragraph listing all keybindings grouped by category.
///
/// # Arguments
/// * `frame` - The frame to render into
/// * `area` - The full terminal area (popup will be centered within it)
pub fn render_help_popup(frame: &mut Frame, area: Rect) {
    let popup_area = centered_popup(60, 70, area);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let heading_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("  Navigation", heading_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled("    j / \u{2193}   ", key_style),
            Span::raw("Move down"),
        ]),
        Line::from(vec![
            Span::styled("    k / \u{2191}   ", key_style),
            Span::raw("Move up"),
        ]),
        Line::from(vec![
            Span::styled("    gg      ", key_style),
            Span::raw("Go to top"),
        ]),
        Line::from(vec![
            Span::styled("    G       ", key_style),
            Span::raw("Go to bottom"),
        ]),
        Line::from(vec![
            Span::styled("    Ngg     ", key_style),
            Span::raw("Go to row N"),
        ]),
        Line::from(vec![
            Span::styled("    Nj / Nk ", key_style),
            Span::raw("Move N rows"),
        ]),
        Line::from(vec![
            Span::styled("    Ctrl-d  ", key_style),
            Span::raw("Half page down"),
        ]),
        Line::from(vec![
            Span::styled("    Ctrl-u  ", key_style),
            Span::raw("Half page up"),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Actions", heading_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled("    Enter   ", key_style),
            Span::raw("Jump to session (tmux)"),
        ]),
        Line::from(vec![
            Span::styled("    r       ", key_style),
            Span::raw("Rescan / refresh"),
        ]),
        Line::from(vec![
            Span::styled("    ?       ", key_style),
            Span::raw("Toggle this help"),
        ]),
        Line::from(vec![
            Span::styled("    q / Esc ", key_style),
            Span::raw("Quit"),
        ]),
    ];

    let popup = Paragraph::new(lines).block(
        Block::default()
            .title(" Help ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(popup, popup_area);
}
