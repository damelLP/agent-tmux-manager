//! Immutable screen projection. Only the last wrapped BODY line qualifies a row.
use atm_comms::{Address, AgentStatus, DeliveryStatus, MessageId};
use atm_tui::ui::theme::context_color;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::state::delivery_label;
use crate::{ChatState, Column, FrameReport, Mode, ThreadKey};

const HELP: &str = "j/k          move in focused column\ngg/G         oldest / newest\nCtrl-d/u     scroll wrapped body lines\nh/l/Tab      threads / messages / seats\nJ/K          next / previous unread thread\nEnter        open thread; pane jump is slice 2\ni            compose in named Direct thread\nr            reply to selected counterpart\n/            literal case-sensitive body search (256 bytes)\nn/N          next / previous search result\ny            copy original body to tmux buffer\n?            help; Esc closes help\nq            close chat\nINSERT: Enter send; Ctrl-j newline; Esc keep draft\nCtrl-r       unknown send: unlock only after 10s\n             send may still arrive; duplicate risk\nprefix C-t   flip separate chat window\nAttachments are references only; never opened.";

/// Escape content controls before they can become terminal cell symbols.
fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() && c != '\n' {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn label(text: &str) -> String {
    safe(text).replace('\n', " ↵ ")
}
fn address(address: &Address) -> String {
    if address == &Address::Owner {
        "you".into()
    } else {
        address.to_string()
    }
}
fn title(thread: &ThreadKey) -> String {
    match thread {
        ThreadKey::ForYou => "For you".into(),
        ThreadKey::All => "All · read only".into(),
        ThreadKey::Direct(target) => target.to_string(),
    }
}
fn agent_state(state: AgentStatus) -> &'static str {
    match state {
        AgentStatus::Starting => "starting",
        AgentStatus::Ready => "ready",
        AgentStatus::Clearing => "clearing",
        AgentStatus::Stuck => "stuck",
        AgentStatus::Down => "down",
    }
}
fn block(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
}

/// Hard wrap using ratatui's own grapheme segmentation and terminal-cell widths.
/// Combining/emoji sequences stay intact, without another Unicode dependency.
fn body_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width);
    if width == 0 {
        return vec![];
    }
    let safe = safe(text);
    let mut lines = vec![];
    for raw in safe.split('\n') {
        let line = Line::raw(raw);
        let mut part = String::new();
        let mut cells: usize = 0;
        for grapheme in line.styled_graphemes(Style::default()) {
            let count = Span::raw(grapheme.symbol).width();
            if cells.saturating_add(count) > width && !part.is_empty() {
                lines.push(Line::raw(std::mem::take(&mut part)));
                cells = 0;
            }
            if count > width {
                part.push('�');
                cells = cells.saturating_add(1);
            } else {
                part.push_str(grapheme.symbol);
                cells = cells.saturating_add(count);
            }
        }
        lines.push(Line::raw(part));
    }
    lines
}

struct BodyEnd {
    id: MessageId,
    start: usize,
    last: usize,
    end: usize,
}
struct Transcript {
    lines: Vec<Line<'static>>,
    bodies: Vec<BodyEnd>,
}
fn transcript(state: &ChatState, width: u16) -> Transcript {
    let mut lines = vec![];
    let mut bodies = vec![];
    let mut divided = false;
    for row in state.view_rows() {
        if !divided
            && row.content.to == Address::Owner
            && row.content.from != Address::Owner
            && row.read_at.is_none()
        {
            lines.push(Line::styled("─ new ─", Style::default().fg(Color::Yellow)));
            divided = true;
        }
        let start = lines.len();
        let selected = state.view().selected == Some(row.message_id);
        let color = if selected {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{} {} {} → {} · ",
                    row.message_id,
                    row.sent_at.format("%H:%M"),
                    label(&address(&row.content.from)),
                    label(&address(&row.content.to))
                ),
                Style::default().fg(color),
            ),
            Span::styled(
                label(&delivery_label(row)),
                Style::default().fg(match row.status {
                    DeliveryStatus::Failed => Color::Red,
                    DeliveryStatus::Delivered => Color::Green,
                    _ => Color::Yellow,
                }),
            ),
        ]));
        let body = body_lines(&row.content.body, width);
        lines.extend(body);
        let last = lines.len().saturating_sub(1);
        for attachment in &row.content.attachments {
            lines.extend(body_lines(&format!("attachment: {attachment}"), width));
        }
        bodies.push(BodyEnd {
            id: row.message_id,
            start,
            last,
            end: lines.len(),
        });
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "No messages in this loaded view",
            Style::default().fg(Color::DarkGray),
        ));
    }
    Transcript { lines, bodies }
}

fn geometry(area: Rect, state: &ChatState) -> (Rect, Rect, Rect, Rect) {
    let left = if area.width >= 100 {
        22
    } else if area.width >= 60 {
        20
    } else {
        0
    };
    let right = if area.width >= 100 { 24 } else { 0 };
    let width = area.width.saturating_sub(left).saturating_sub(right);
    let composer_height = if state.mode == Mode::Insert || state.view().draft.contains('\n') {
        4
    } else {
        3
    };
    let content_height = area.height.saturating_sub(4);
    let transcript_height = content_height.saturating_sub(composer_height);
    let middle = Rect::new(
        area.x.saturating_add(left),
        area.y.saturating_add(1),
        width,
        transcript_height,
    );
    let composer = Rect::new(
        middle.x,
        middle.y.saturating_add(transcript_height),
        width,
        composer_height.min(content_height),
    );
    let left = Rect::new(area.x, middle.y, left, content_height);
    let right = Rect::new(
        middle.x.saturating_add(width),
        middle.y,
        right,
        content_height,
    );
    (left, middle, right, composer)
}

pub(crate) fn scroll_limit(state: &ChatState) -> usize {
    let (_, middle, _, _) = geometry(Rect::new(0, 0, state.size.0, state.size.1), state);
    let inner = block("", false).inner(middle);
    let transcript = transcript(state, inner.width);
    let start = transcript
        .bodies
        .iter()
        .find(|b| Some(b.id) == state.view().selected)
        .map(|b| b.start)
        .unwrap_or(0);
    transcript
        .lines
        .len()
        .saturating_sub(usize::from(inner.height))
        .saturating_sub(start)
}

/// Pure rendering; focus evidence and mutation effects belong to the controller/reducer.
#[must_use]
pub fn render(frame: &mut ratatui::Frame<'_>, state: &ChatState) -> FrameReport {
    let mut report = FrameReport {
        generation: state.generation,
        thread: state.thread.clone(),
        visible_ids: vec![],
        read_ids: vec![],
        can_compose: state.can_compose(),
    };
    let area = frame.area();
    if area.width < 20 || area.height < 8 {
        frame.render_widget(Paragraph::new("Resize chat to at least 20 × 8"), area);
        report.can_compose = false;
        return report;
    }
    if state.mode == Mode::Help {
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(HELP)
                .block(block("Chat help · slice 1", true))
                .wrap(Wrap { trim: false }),
            area,
        );
        report.can_compose = false;
        return report;
    }
    let (left, middle, right, composer) = geometry(area, state);
    let selected_seat = if let ThreadKey::Direct(Address::Agent(agent)) = &state.thread {
        state.seats.iter().find(|s| &s.agent == agent)
    } else {
        None
    };
    let mut heading = title(&state.thread);
    if let Some(seat) = selected_seat {
        heading.push_str(&format!(
            " · {} · {}{}",
            agent_state(seat.state),
            label(&seat.activity_label),
            if seat.binding.pane.is_none() {
                " · pane missing"
            } else {
                ""
            }
        ));
    } else if matches!(state.thread, ThreadKey::Direct(Address::Agent(_))) {
        heading.push_str(" · gone/unknown seat · read only");
    }
    if state.view().stale {
        heading.push_str(" · stale");
    }
    frame.render_widget(
        Paragraph::new(label(&heading)).style(Style::default().add_modifier(Modifier::BOLD)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if left.width > 0 {
        let mut lines = vec![Line::styled(
            format!(
                "{}For you  {} unread",
                if state.column == Column::Threads && state.thread_index == 0 {
                    "› "
                } else {
                    ""
                },
                state.unread_count
            ),
            Style::default().fg(Color::Yellow),
        )];
        if state.summary_incomplete {
            lines.push(Line::raw("first page only"));
        }
        if state.decisions_truncated {
            lines.push(Line::raw(format!(
                "at least {} decisions",
                state.waiting_decisions
            )));
            lines.push(Line::raw("summary capped"));
        } else {
            lines.push(Line::raw(format!("decisions: {}", state.waiting_decisions)));
        }
        lines.push(Line::raw("Direct"));
        for (index, thread) in state
            .threads()
            .into_iter()
            .enumerate()
            .filter(|(_, t)| matches!(t, ThreadKey::Direct(_)))
        {
            let count = if let ThreadKey::Direct(address) = &thread {
                state.unread_by_sender.get(address).copied().unwrap_or(0)
            } else {
                0
            };
            lines.push(Line::styled(
                format!(
                    "{} {}{}",
                    if state.column == Column::Threads && state.thread_index == index {
                        "›"
                    } else if state.thread == thread {
                        "▸"
                    } else {
                        " "
                    },
                    label(&title(&thread)),
                    if count > 0 {
                        format!("  {count}")
                    } else {
                        String::new()
                    }
                ),
                Style::default().fg(if state.thread == thread {
                    Color::Cyan
                } else {
                    Color::White
                }),
            ));
        }
        lines.push(Line::raw(
            if state.column == Column::Threads
                && state.thread_index == state.threads().len().saturating_sub(1)
            {
                "› All · read only"
            } else {
                "All · read only"
            },
        ));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block("Threads", state.column == Column::Threads))
                .wrap(Wrap { trim: false }),
            left,
        );
    }
    let border = block(
        "Messages · oldest ↑ / newest ↓",
        state.column == Column::Messages,
    );
    let inner = border.inner(middle);
    frame.render_widget(border, middle);
    let transcript = transcript(state, inner.width);
    let height = usize::from(inner.height);
    let selected = transcript
        .bodies
        .iter()
        .find(|b| Some(b.id) == state.view().selected);
    let anchor = selected
        .map(|body| {
            if state.view().scroll > 0 {
                body.start.saturating_add(state.view().scroll)
            } else if body.end.saturating_sub(body.start) > height {
                body.start
            } else {
                body.end.saturating_sub(height)
            }
        })
        .unwrap_or(0);
    let offset = anchor.min(transcript.lines.len().saturating_sub(height));
    if height > 0 && inner.width > 0 {
        for body in &transcript.bodies {
            if body.last >= offset && body.last < offset.saturating_add(height) {
                report.visible_ids.push(body.id);
                if state.row(&body.id).is_some_and(|row| row.read_at.is_some()) {
                    report.read_ids.push(body.id);
                }
            }
        }
        let visible: Vec<_> = transcript
            .lines
            .into_iter()
            .skip(offset)
            .take(height)
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);
    }
    let composer_title = match state.mode {
        Mode::Insert => "INSERT · Enter send · Ctrl-j newline · Esc keep",
        Mode::Search => "SEARCH · literal body · Enter apply · Esc cancel",
        _ if state.draft_locked() => "Draft locked · outcome unknown/pending",
        _ => "Draft · i compose · All/System/self read only",
    };
    let content = if state.mode == Mode::Search {
        state.search_edit.as_str()
    } else {
        state.view().draft.as_str()
    };
    frame.render_widget(
        Paragraph::new(safe(content))
            .block(block(
                composer_title,
                state.mode == Mode::Insert || state.mode == Mode::Search,
            ))
            .wrap(Wrap { trim: false }),
        composer,
    );
    if right.width > 0 {
        let mut lines = vec![];
        for (index, seat) in state.seats.iter().enumerate() {
            lines.push(Line::styled(
                format!(
                    "{}{}",
                    if state.column == Column::Seats && state.seat_index == index {
                        "› "
                    } else {
                        ""
                    },
                    seat.agent
                ),
                Style::default()
                    .fg(seat.activity_color)
                    .add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::raw(format!(
                "{} · {}",
                agent_state(seat.state),
                label(&seat.activity_label)
            )));
            if seat.binding.pane.is_none() {
                lines.push(Line::styled(
                    "pane missing",
                    Style::default().fg(Color::Yellow),
                ));
            }
            if let Some(pct) = seat.context_percentage {
                lines.push(Line::styled(
                    format!("context {pct:.0}%"),
                    Style::default().fg(context_color(pct, false)),
                ));
            }
            if let Some(note) = state.turn_notes.get(&seat.agent) {
                lines.push(Line::raw(note.clone()));
            }
            lines.push(Line::raw(""));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(block("Current seats", state.column == Column::Seats))
                .wrap(Wrap { trim: false }),
            right,
        );
    }
    let status = if state.has_unknown_send() && !state.status.contains("send may still arrive") {
        format!("send may still arrive · {}", state.status)
    } else {
        state.status.clone()
    };
    for (row, text, color) in [
        (
            area.height.saturating_sub(3),
            status.as_str(),
            Color::Yellow,
        ),
        (
            area.height.saturating_sub(2),
            state
                .read_hint
                .as_deref()
                .unwrap_or("focused frame required · last wrapped body line = read"),
            Color::DarkGray,
        ),
        (
            area.height.saturating_sub(1),
            "j/k · gg/G · Ctrl-d/u · i write · r reply · / search · ? help · q close",
            Color::DarkGray,
        ),
    ] {
        frame.render_widget(
            Paragraph::new(label(text)).style(Style::default().fg(color)),
            Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
        );
    }
    report
}
