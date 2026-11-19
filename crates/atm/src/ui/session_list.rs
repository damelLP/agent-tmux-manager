//! Session list widget for the ATM TUI.
//!
//! Displays a scrollable list of Claude Code sessions with
//! real-time status updates including context usage, cost, and duration.

use crate::app::{App, AppState, AppView};
use crate::ui::theme::{context_color, status_background, status_color, status_icon};
use crate::work::{work_marks, PaneId, SeatId, WindowId, WindowMarks, WorkGrouping, WorkMarks};
use atm_core::{SessionView, TreeRow, TreeRowKind};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
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
    if render_list_state(frame, area, app) {
        return;
    }
    let marks = view_marks(app);

    let items: Vec<ListItem> = app
        .tree_rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let item = create_tree_row_item(
                row,
                idx == app.selected_index,
                app,
                &marks,
                area.width.saturating_sub(2),
            );
            dim_if_filter_context(item, row, app)
        })
        .collect();

    let title = list_title(app, area.width);

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
    if render_list_state(frame, area, app) {
        return;
    }
    let marks = view_marks(app);

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
                TreeRowKind::Team { name } if app.view == AppView::Work => {
                    create_thread_line(&indent, name, row, is_selected, app, &marks, inner_width)
                }
                TreeRowKind::Team { name } => create_group_line(&indent, name, row, is_selected),
                TreeRowKind::Window { id, label } => create_window_line(
                    &indent,
                    (id, label),
                    row,
                    is_selected,
                    app,
                    &marks,
                    inner_width,
                ),
                TreeRowKind::Agent { session } => create_view_agent_line(
                    &indent,
                    session,
                    is_selected,
                    app,
                    &marks,
                    Some(inner_width),
                    inner_width,
                ),
            };

            let bg_style = match &row.kind {
                TreeRowKind::Agent { session } => get_row_background_style(session, is_selected),
                _ if is_selected => Style::default().bg(Color::Rgb(30, 30, 40)),
                _ => Style::default(),
            };

            dim_if_filter_context(ListItem::new(line).style(bg_style), row, app)
        })
        .collect();

    let title = list_title(app, area.width);

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::White)),
    );

    frame.render_widget(list, area);
}

fn list_title(app: &App, width: u16) -> Line<'static> {
    let tab = |label, view| {
        Span::styled(
            label,
            if app.view == view {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM)
            },
        )
    };
    let mut line = Line::from(vec![
        Span::raw(" "),
        tab("Work", AppView::Work),
        Span::raw(" | "),
        tab("Sessions", AppView::Sessions),
    ]);
    if app.view == AppView::Sessions {
        line.spans
            .push(Span::raw(format!(" ({}) ", app.session_count())));
        return line;
    }
    let (windows, agents) = app.work_counts();
    let unavailable = match (app.work.windows_unavailable(), app.work.org_unavailable()) {
        (true, true) => " · windows/org unavailable",
        (true, false) => " · windows unavailable",
        (false, true) => " · org unavailable",
        (false, false) => "",
    };
    let unavailable = if width < 60 && !unavailable.is_empty() {
        " · unavailable"
    } else {
        unavailable
    };
    let threads = if app.work_grouping == WorkGrouping::Threads && app.work.org().is_some() {
        format!(" · {} threads", app.work_thread_count())
    } else {
        String::new()
    };
    let mut title = format!("{unavailable}{threads} · {windows} windows, {agents} agents");
    if app.work.no_org_data() {
        title.push_str(" · no org data");
    }
    if let Some(age) = app.work.stale_age(app.work_now) {
        title.push_str(&format!(" · stale {}s", age.as_secs()));
    }
    title.push(' ');
    line.spans.push(Span::raw(title));
    line
}
fn render_list_state(frame: &mut Frame, area: Rect, app: &App) -> bool {
    let rejected = matches!(
        app.state,
        AppState::Disconnected {
            reason: Some(_),
            ..
        }
    );
    if shows_empty_state(app) && (app.view == AppView::Sessions || rejected) {
        render_empty_state(frame, area, &app.state, list_title(app, area.width));
        return true;
    }
    if app.view == AppView::Work && app.work.needs_newer_daemon() {
        frame.render_widget(
            Paragraph::new("Work needs a newer atmd. w: Sessions")
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(list_title(app, area.width)),
                ),
            area,
        );
        return true;
    }
    false
}
fn view_marks(app: &App) -> WorkMarks {
    if app.view == AppView::Work {
        app.work
            .org()
            .map(|org| {
                let mut marks = work_marks(org, app.work.windows());
                if app.work_grouping == WorkGrouping::Threads {
                    for window in app.work.windows() {
                        let total = marks.by_window.entry(window.id.clone()).or_default();
                        *total = WindowMarks::default();
                        for pane in window
                            .panes
                            .iter()
                            .collect::<std::collections::BTreeSet<_>>()
                        {
                            if let Some(mark) = marks
                                .by_pane
                                .get(pane)
                                .filter(|m| !marks.threads.contains_key(&m.agent_id))
                            {
                                total.include(mark);
                            }
                        }
                    }
                }
                marks
            })
            .unwrap_or_default()
    } else {
        WorkMarks::default()
    }
}
fn work_text(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_debug().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
// Work clips escaped text by display cells; Sessions keeps its existing truncation contract.
fn clip_work(text: &str, width: usize) -> String {
    let text = work_text(text);
    if Span::raw(&text).width() <= width {
        return text;
    }
    let ellipsis = if width > 3 { "..." } else { "" };
    let mut remaining = width.saturating_sub(ellipsis.len());
    let mut clipped = String::new();
    for c in text.chars() {
        let cells = Span::raw(c.to_string()).width();
        if cells > remaining {
            break;
        }
        clipped.push(c);
        remaining = remaining.saturating_sub(cells);
    }
    clipped.push_str(ellipsis);
    clipped
}

fn work_caption(
    mut prefix: Line<'static>,
    headline: &str,
    badges: Line<'static>,
    detail: String,
    width: u16,
) -> Line<'static> {
    let budget = usize::from(width)
        .saturating_sub(prefix.width())
        .saturating_sub(badges.width());
    prefix.spans.push(Span::styled(
        clip_work(headline, budget),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    prefix.spans.extend(badges.spans);
    let remaining = usize::from(width).saturating_sub(prefix.width());
    prefix.spans.push(Span::styled(
        clip_work(&detail, remaining),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ));
    prefix
}

fn group_badges(row: &TreeRow, mark: Option<&WindowMarks>) -> Line<'static> {
    let mut spans = Vec::new();
    if !row.is_expanded && row.agent_count > 0 {
        spans.push(Span::styled(
            format!(" ({})", row.agent_count),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(mark) = mark {
        if mark.tasks_open > 0 {
            spans.push(Span::styled(
                format!(" {} open", mark.tasks_open),
                Style::default().fg(Color::Cyan),
            ));
        }
        if mark.decisions_waiting > 0 {
            spans.push(Span::styled(
                format!(" ?{}", mark.decisions_waiting),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    if row.needs_attention || mark.is_some_and(|m| m.attention) {
        spans.push(Span::styled(" !", Style::default().fg(Color::Yellow)));
    }
    Line::from(spans)
}

fn append_work_detail(detail: &mut String, part: &str) {
    if !part.is_empty() {
        detail.push_str(" · ");
        detail.push_str(part);
    }
}

fn thread_headline<'a>(name: &'a str, marks: &'a WorkMarks) -> &'a str {
    marks
        .by_seat
        .get(&SeatId(name.into()))
        .map(|m| m.task_headline.as_deref().unwrap_or(&m.name))
        .unwrap_or(name)
}

fn create_thread_line(
    indent: &str,
    name: &str,
    row: &TreeRow,
    selected: bool,
    app: &App,
    marks: &WorkMarks,
    width: u16,
) -> Line<'static> {
    let id = SeatId(name.into());
    let headline = thread_headline(name, marks);
    let repos: std::collections::BTreeSet<_> = marks
        .threads
        .iter()
        .filter(|(_, lead)| *lead == &id)
        .filter_map(|(seat, _)| seat.0.rsplit_once('/').map(|(repo, _)| repo))
        .filter(|repo| !repo.is_empty())
        .collect();
    let windows: Vec<_> = app
        .work
        .windows()
        .iter()
        .filter(|window| {
            window.panes.iter().any(|pane| {
                marks
                    .by_pane
                    .get(pane)
                    .and_then(|m| marks.threads.get(&m.agent_id))
                    == Some(&id)
            })
        })
        .map(|w| w.name.as_str())
        .filter(|name| !name.is_empty())
        .collect();
    let short = marks.by_seat.get(&id).map_or(name, |m| m.name.as_str());
    let mut detail = String::new();
    if headline != short {
        append_work_detail(&mut detail, short);
    }
    append_work_detail(
        &mut detail,
        &repos.into_iter().collect::<Vec<_>>().join(", "),
    );
    append_work_detail(&mut detail, &windows.join(", "));
    work_group_line(
        indent,
        headline,
        row,
        selected,
        marks.by_thread.get(&id),
        detail,
        width,
    )
}

fn work_group_line(
    indent: &str,
    headline: &str,
    row: &TreeRow,
    selected: bool,
    mark: Option<&WindowMarks>,
    detail: String,
    width: u16,
) -> Line<'static> {
    let icon = if !row.has_children {
        " "
    } else if row.is_expanded {
        "▼"
    } else {
        "▸"
    };
    let prefix = Line::from(vec![
        Span::styled(
            if selected { ">" } else { " " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{indent}{icon} ")),
    ]);
    work_caption(prefix, headline, group_badges(row, mark), detail, width)
}

fn create_window_line(
    indent: &str,
    window: (&str, &str),
    row: &TreeRow,
    selected: bool,
    app: &App,
    marks: &WorkMarks,
    width: u16,
) -> Line<'static> {
    let (id, label) = window;
    let leads = (app.work_grouping == WorkGrouping::Windows)
        .then(|| app.work.windows().iter().find(|w| w.id.0 == id))
        .flatten()
        .map(|window| {
            window
                .panes
                .iter()
                .filter_map(|pane| {
                    marks
                        .by_pane
                        .get(pane)
                        .and_then(|m| marks.threads.get(&m.agent_id))
                })
                .collect::<std::collections::BTreeSet<_>>()
        });
    let (headline, detail) = match leads.as_ref() {
        Some(leads) if leads.len() == 1 => leads.first().map_or((label, String::new()), |lead| {
            (
                thread_headline(&lead.0, marks),
                format!(" · {} · {label}", lead.0),
            )
        }),
        Some(leads) if leads.len() > 1 => (
            "Work threads",
            format!(
                " · {} · {label}",
                leads
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        _ => (label, String::new()),
    };
    work_group_line(
        indent,
        headline,
        row,
        selected,
        marks.by_window.get(&WindowId(id.into())),
        detail,
        width,
    )
}
fn create_view_agent_line(
    indent: &str,
    session: &SessionView,
    selected: bool,
    app: &App,
    marks: &WorkMarks,
    compact: Option<u16>,
    width: u16,
) -> Line<'static> {
    let work = app.view == AppView::Work;
    // In-process children share a pane; only a parent-less row may impersonate its seat.
    let mark = if work && session.parent_session_id.is_none() {
        session
            .tmux_pane
            .as_ref()
            .and_then(|pane| marks.by_pane.get(&PaneId(pane.clone())))
    } else {
        None
    };
    let attention = mark.is_some_and(|mark| mark.attention);
    let waiting =
        app.has_waiting_decision(session) || mark.is_some_and(|mark| mark.decisions_waiting > 0);
    let coordinator = app.is_coordinator(session);
    if !work {
        return match compact {
            Some(width) => create_compact_agent_line(
                indent,
                session,
                selected,
                app.blink_visible,
                width,
                coordinator,
                waiting,
            ),
            None => create_agent_line(
                indent,
                session,
                selected,
                app.blink_visible,
                coordinator,
                waiting,
            ),
        };
    }
    let name = mark.map_or(session.id_short.as_str(), |m| m.name.as_str());
    let task = mark.and_then(|m| m.first_task.as_ref());
    let task_headline = mark.and_then(|m| m.task_headline.as_deref());
    let headline = task_headline.unwrap_or(name);
    let mut badges = Vec::new();
    if let Some(mark) = mark.filter(|m| m.tasks_open > 1) {
        badges.push(Span::styled(
            format!(" +{}", mark.tasks_open.saturating_sub(1)),
            Style::default().fg(Color::Cyan),
        ));
    }
    if waiting {
        let count = mark.map_or(1, |m| m.decisions_waiting.max(1));
        badges.push(Span::styled(
            format!(" ?{count}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if attention {
        badges.push(Span::styled(" !", Style::default().fg(Color::Yellow)));
    }
    if coordinator {
        badges.push(Span::styled(" C", Style::default().fg(Color::Cyan)));
    }
    let prefix = Line::from(vec![
        Span::styled(
            if selected { ">" } else { " " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(indent.to_string()),
        Span::styled(
            format!("{} ", status_icon(session.status, app.blink_visible)),
            Style::default()
                .fg(status_color(session.status))
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    if let Some(task) = task.filter(|_| task_headline.is_none()) {
        // Capped IDs are useful detail, but may not push required count markers off-screen.
        let budget = usize::from(width)
            .saturating_sub(prefix.width())
            .saturating_sub(Span::raw(work_text(headline)).width())
            .saturating_sub(badges.iter().map(Span::width).sum());
        if budget > 0 {
            badges.insert(
                0,
                Span::styled(
                    clip_work(&format!(" ◐ {}", task.id.0), budget),
                    Style::default().fg(Color::Cyan),
                ),
            );
        }
    }
    let agent = if headline == name || name.is_empty() {
        String::new()
    } else {
        format!(" · {name}")
    };
    let repo = session
        .project_root
        .as_deref()
        .and_then(|p| p.rsplit('/').find(|s| !s.is_empty()));
    let branch = session
        .worktree_branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
        .or_else(|| {
            session
                .worktree_path
                .as_deref()
                .and_then(|p| p.rsplit('/').find(|s| !s.is_empty()))
        });
    let mut detail = format!(
        "{agent} [{}] {:.0}%",
        harness_label(&session.harness),
        session.context_percentage
    );
    if let Some(repo) = repo {
        append_work_detail(&mut detail, repo);
        if let Some(branch) = branch {
            detail.push('/');
            detail.push_str(branch);
        }
    } else if let Some(branch) = branch {
        append_work_detail(&mut detail, branch);
    }
    work_caption(prefix, headline, Line::from(badges), detail, width)
}

/// Dims an agent row shown only as the ancestor of a filter match.
fn dim_if_filter_context<'a>(item: ListItem<'a>, row: &TreeRow, app: &App) -> ListItem<'a> {
    match &row.kind {
        TreeRowKind::Agent { session } if app.is_filter_context(&session.id) => item.dim(),
        _ => item,
    }
}

/// Creates a list item for a single tree row.
fn create_tree_row_item(
    row: &TreeRow,
    is_selected: bool,
    app: &App,
    marks: &WorkMarks,
    width: u16,
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
        TreeRowKind::Team { name } if app.view == AppView::Work => {
            create_thread_line(&indent, name, row, is_selected, app, marks, width)
        }
        TreeRowKind::Team { name } => create_group_line(&indent, name, row, is_selected),
        TreeRowKind::Window { id, label } => {
            create_window_line(&indent, (id, label), row, is_selected, app, marks, width)
        }
        TreeRowKind::Agent { session } => {
            create_view_agent_line(&indent, session, is_selected, app, marks, None, width)
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
    coordinator: bool,
    waiting: bool,
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
        decision_span(waiting),
        coordinator_span(coordinator),
        // Harness badge: `[pi]`, `[claude]`, `[?]`. Rendered in
        // a distinct color so vendor differences are scannable.
        Span::styled(
            format!("[{}]", harness_label(&session.harness)),
            Style::default().fg(harness_color(&session.harness)),
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

fn decision_span(waiting: bool) -> Span<'static> {
    Span::styled(
        if waiting { "? " } else { "" },
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

fn coordinator_span(coordinator: bool) -> Span<'static> {
    Span::styled(
        if coordinator { "C " } else { "" },
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

/// Display label for the harness badge. Empty string (legacy sessions
/// from before this field existed) defaults to `claude` since that was
/// the only supported harness.
fn harness_label(harness: &str) -> &str {
    if harness.is_empty() {
        "claude"
    } else {
        harness
    }
}

/// Per-harness badge color so users can scan the list for non-Claude
/// sessions at a glance.
fn harness_color(harness: &str) -> Color {
    match harness {
        "pi" => Color::Magenta,
        "codex" => Color::Cyan,
        "claude" | "" => Color::Yellow,
        _ => Color::DarkGray,
    }
}

/// Creates a compact line for an agent row in narrow sidebars.
///
/// Format: `{selector}{indent}{icon} {ctx%} {optional C }{name}`
/// Name is resolved from worktree branch (preferred) or short ID (fallback),
/// then adaptively truncated to fit `available_width`.
fn create_compact_agent_line(
    indent: &str,
    session: &SessionView,
    is_selected: bool,
    blink_visible: bool,
    available_width: u16,
    coordinator: bool,
    waiting: bool,
) -> Line<'static> {
    let icon = status_icon(session.status, blink_visible);
    let icon_color = status_color(session.status);
    let ctx_color = context_color(session.context_percentage, session.context_critical);

    let name = session
        .worktree_branch
        .as_deref()
        .unwrap_or(&session.id_short);

    let overhead = 9u16
        .saturating_add(indent.len() as u16)
        .saturating_add(if coordinator { 2 } else { 0 })
        .saturating_add(if waiting { 2 } else { 0 });
    let name_width = available_width.saturating_sub(overhead) as usize;
    let truncated_name = truncate_string(name, name_width.max(1));

    let spans = vec![
        Span::styled(
            if is_selected { ">" } else { " " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
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
        decision_span(waiting),
        coordinator_span(coordinator),
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

/// Whether the list area shows the empty state: there are no sessions,
/// or the daemon rejects this client, so the sessions shown would be stale.
fn shows_empty_state(app: &App) -> bool {
    app.sessions.is_empty()
        || matches!(
            app.state,
            AppState::Disconnected {
                reason: Some(_),
                ..
            }
        )
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
fn render_empty_state(frame: &mut Frame, area: Rect, state: &AppState, title: Line<'static>) {
    let (_, lines) = match state {
        AppState::Connected => (
            " No Sessions ",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No active agent sessions detected",
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
                    "  2. Run an agent",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(Span::styled(
                    "  3. Session will appear here automatically",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Tip: Run atm setup to add atm integration to your agents",
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
        AppState::Disconnected {
            retry_count,
            reason,
            ..
        } => (
            " Disconnected ",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    reason.as_deref().unwrap_or("Lost connection to daemon"),
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
                    "  3. Restart: atmd stop, rerun atm",
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
    // A rejection reason is wider than the list, so let it wrap
    let paragraph = match state {
        AppState::Disconnected {
            reason: Some(_), ..
        } => paragraph.wrap(Wrap { trim: false }),
        _ => paragraph,
    };

    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{WorkEffect, WorkInput, WorkOrg, WorkWindow, WorkWindows};
    use atm_core::{SessionStatus, TreeNodeId};
    use serde_json::json;
    use std::time::Duration;

    fn load_caption_windows(app: &mut App, windows: Vec<WorkWindow>) {
        let generation = app
            .work
            .reduce(WorkInput::Shown, Duration::ZERO)
            .into_iter()
            .find_map(|effect| match effect {
                WorkEffect::RequestWindows(generation) => Some(generation),
                _ => None,
            })
            .unwrap();
        let _ = app.work.reduce(
            WorkInput::WindowsReply {
                generation,
                result: Ok(WorkWindows {
                    state: "ok".into(),
                    windows,
                }),
            },
            Duration::ZERO,
        );
    }

    fn expanded_group(node_id: TreeNodeId, kind: TreeRowKind) -> TreeRow {
        TreeRow {
            depth: 0,
            node_id,
            kind,
            is_expanded: true,
            agent_count: 1,
            needs_attention: false,
            has_children: true,
        }
    }

    // Break caught: unknown panes erase known leads, or a mixed window picks a lead by pane order.
    #[test]
    fn window_headline_ignores_unattributed_panes() {
        let org: WorkOrg = serde_json::from_value(json!({"projects":[{
            "seats":[
                {"agent_id":"alpha/lead","pane":"%1","manager":"owner","tasks":["t1"]},
                {"agent_id":"alpha/one","pane":"%2","manager":"alpha/lead","tasks":[]},
                {"agent_id":"alpha/two","pane":"%4","manager":"alpha/lead","tasks":[]},
                {"agent_id":"beta/lead","pane":"%5","manager":"owner","tasks":[]},
                {"agent_id":"beta/one","pane":null,"manager":"beta/lead","tasks":[]}
            ],"tasks":[{"task_id":"t1","title":"Alpha task"}]
        }],"needs_owner":{"items":[]}}))
        .unwrap();
        let mut app = App::new();
        load_caption_windows(
            &mut app,
            serde_json::from_value(json!([
                {"id":"@1","name":"one","links":[],"panes":["%1","%2","%3"]},
                {"id":"@2","name":"mix","links":[],"panes":["%6","%5","%4"]},
                {"id":"@3","name":"plain","links":[],"panes":["%7"]}
            ]))
            .unwrap(),
        );
        let marks = work_marks(&org, app.work.windows());
        for (id, label, headline, detail, badge) in [
            (
                "@1",
                "1 one",
                "Alpha task",
                " · alpha/lead · 1 one",
                " 1 open",
            ),
            (
                "@2",
                "2 mix",
                "Work threads",
                " · alpha/lead, beta/lead · 2 mix",
                "",
            ),
            ("@3", "3 plain", "3 plain", "", ""),
        ] {
            let row = expanded_group(
                TreeNodeId::Window(id.into()),
                TreeRowKind::Window {
                    id: id.into(),
                    label: label.into(),
                },
            );
            app.work_grouping = WorkGrouping::Windows;
            let line = create_window_line("", (id, label), &row, false, &app, &marks, 100);
            assert_eq!(line.to_string(), format!(" ▼ {headline}{badge}{detail}"));
            let secondary = line.spans.last().unwrap();
            assert_eq!(secondary.content, detail);
            assert_eq!(secondary.style.fg, Some(Color::DarkGray));
            assert!(secondary.style.add_modifier.contains(Modifier::DIM));

            app.work_grouping = WorkGrouping::Threads;
            assert_eq!(
                create_window_line("", (id, label), &row, false, &app, &marks, 100).to_string(),
                format!(" ▼ {label}{badge}"),
            );
        }
    }

    // Break caught: missing path components leave a slash or a dangling separator in Work.
    #[test]
    fn work_agent_detail_joins_present_repo_and_branch() {
        let app = App::new();
        let marks = WorkMarks::default();
        for (project, branch, worktree, want) in [
            (
                Some("/fixtures/repo"),
                Some("main"),
                None,
                " [pi] 0% · repo/main",
            ),
            (Some("/fixtures/repo"), None, None, " [pi] 0% · repo"),
            (None, Some("dev"), None, " [pi] 0% · dev"),
            (None, None, None, " [pi] 0%"),
            (
                Some("/fixtures/repo"),
                Some(""),
                Some("/fixtures/tree/"),
                " [pi] 0% · repo/tree",
            ),
            (
                Some("/fixtures/repo"),
                Some("cp/work-a"),
                None,
                " [pi] 0% · repo/cp/work-a",
            ),
        ] {
            let session = SessionView {
                id_short: "r".into(),
                harness: "pi".into(),
                status: SessionStatus::Idle,
                project_root: project.map(Into::into),
                worktree_branch: branch.map(Into::into),
                worktree_path: worktree.map(Into::into),
                ..Default::default()
            };
            let line = create_view_agent_line("  ", &session, false, &app, &marks, None, 100);
            assert_eq!(
                line.spans.last().unwrap().content,
                want,
                "{project:?}/{branch:?}/{worktree:?}"
            );
        }

        let session = SessionView {
            id_short: "r".into(),
            harness: "pi".into(),
            status: SessionStatus::Idle,
            tmux_pane: Some("%1".into()),
            ..Default::default()
        };
        for (title, want) in [("Work item", " · r [pi] 0%"), ("r", " [pi] 0%")] {
            let org: WorkOrg = serde_json::from_value(json!({"projects":[{
                "seats":[{"agent_id":"repo/r","pane":"%1","tasks":["t1"]}],
                "tasks":[{"task_id":"t1","title":title}]
            }],"needs_owner":{"items":[]}}))
            .unwrap();
            let marks = work_marks(&org, &[]);
            let line = create_view_agent_line("  ", &session, false, &app, &marks, None, 100);
            assert_eq!(line.spans.last().unwrap().content, want, "{title}");
        }
    }

    // Break caught: absent name/repo/window chunks add dots or an empty comma item.
    #[test]
    fn work_thread_detail_joins_present_name_repos_and_windows() {
        for (id, title, with_windows, want) in [
            ("lead", None, false, ""),
            ("repo/solo", None, false, " · repo"),
            ("lead", None, true, " · ops"),
            ("repo/lead", Some("Work item"), true, " · lead · repo · ops"),
        ] {
            let mut app = App::new();
            let windows = if with_windows {
                serde_json::from_value(json!([
                    {"id":"@1","name":"ops","links":[],"panes":["%5"]},
                    {"id":"@2","name":"","links":[],"panes":["%6"]}
                ]))
                .unwrap()
            } else {
                vec![]
            };
            load_caption_windows(&mut app, windows);
            let tasks = title
                .map(|s| json!([{"task_id":"t1","title":s}]))
                .unwrap_or(json!([]));
            let assigned = if title.is_some() { vec!["t1"] } else { vec![] };
            let org: WorkOrg = serde_json::from_value(json!({"projects":[{
                "seats":[
                    {"agent_id":id,"pane":"%5","manager":"owner","tasks":assigned},
                    {"agent_id":"/worker","pane":"%6","manager":id,"tasks":[]}
                ],"tasks":tasks
            }],"needs_owner":{"items":[]}}))
            .unwrap();
            let marks = work_marks(&org, app.work.windows());
            let row = expanded_group(
                TreeNodeId::Team(id.into()),
                TreeRowKind::Team { name: id.into() },
            );
            let line = create_thread_line("", id, &row, false, &app, &marks, 100);
            assert_eq!(
                line.spans.last().unwrap().content,
                want,
                "{id}, windows={with_windows}"
            );
        }
    }

    #[test]
    fn coordinator_span_precedes_badge_or_name_without_reserving_untagged_space() {
        let session = SessionView {
            id_short: "short-id".into(),
            model: "model".into(),
            harness: "pi".into(),
            worktree_branch: Some("abcdefghijklmnop".into()),
            ..SessionView::default()
        };
        let full = create_agent_line("  ", &session, false, true, true, false);
        let plain = create_agent_line("  ", &session, false, true, false, false);
        assert_eq!(full.width(), plain.width() + 2);
        assert!(full.to_string().contains("short-id C [pi]"));
        assert!(plain.to_string().contains("short-id [pi]"));
        for tagged in [false, true] {
            let line = create_compact_agent_line("  ", &session, false, true, 20, tagged, false);
            assert_eq!(line.width(), 20);
            let suffix = if tagged { "C abcd..." } else { "abcdef..." };
            assert!(line.to_string().ends_with(suffix), "{line}");
            for span in line.spans.iter().filter(|span| span.content == "C ") {
                assert_eq!(span.style.fg, Some(Color::Cyan));
                assert!(span.style.add_modifier.contains(Modifier::BOLD));
            }
        }
    }

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
