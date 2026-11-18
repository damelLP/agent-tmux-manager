//! Snapshot tests for ATM TUI widgets.
//!
//! Each test renders a single widget into a fixed-size `TestBackend` and
//! snapshots the resulting `Buffer` via `insta::assert_debug_snapshot!`,
//! which captures both cell contents AND styles (fg/bg/modifier). That
//! lets us catch color/bold regressions in addition to layout/text drift —
//! `TestBackend`'s `Display` impl strips styles, so we use `Debug` instead.
//!
//! To regenerate snapshots after an intentional change:
//!     INSTA_UPDATE=always cargo test -p atm-tui --test ui_snapshots
//! Then visually inspect the diff (`cargo insta review`) before committing.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use std::{sync::Mutex, time::Duration};

use atm_core::{SessionId, SessionStatus, SessionView};
use atm_tui::app::{App, AppState};
use atm_tui::beads::BeadsTask;
use atm_tui::error::TuiError;
use atm_tui::ui::{
    detail_panel::{render_compact_preview, render_detail_panel_inline},
    help_popup::render_help_popup,
    session_list::{render_compact_session_list, render_session_list},
    status_bar::{render_compact_footer, render_footer, render_header},
};
use atm_tui::work::{WorkEffect, WorkInput, WorkOrg, WorkWindows};
use chrono::{DateTime, Utc};
use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};

/// Serializes the `#[test]` functions in this binary that mutate
/// process-wide environment variables. Cargo compiles `tests/ui_snapshots.rs`
/// into its own binary, but the `#[test]`s inside it run concurrently by
/// default — without this lock, env mutation in one test would race with
/// `tmux::is_in_tmux()` reads in another.
///
/// The mutex is recovered from poisoning in `with_tmux` so a single panicking
/// test doesn't cascade into spurious failures for the others.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that restores `TMUX` to its previous value when dropped.
/// Drop runs even on panic, so the env state stays consistent across tests.
struct TmuxEnvGuard {
    prev: Option<String>,
}

impl Drop for TmuxEnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("TMUX", v),
            None => std::env::remove_var("TMUX"),
        }
    }
}

/// Runs `f` with `TMUX` set to a fixed value, restoring the previous value
/// afterward — even if `f` panics. Used for widgets whose output depends on
/// `tmux::is_in_tmux()`.
fn with_tmux<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    // Recover from poisoning: if a previous test panicked while holding the
    // lock, its TmuxEnvGuard already restored TMUX on unwind, so the env
    // state is consistent and we can safely proceed.
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = TmuxEnvGuard {
        prev: std::env::var("TMUX").ok(),
    };
    std::env::set_var("TMUX", "/tmp/snapshot-test");
    f()
}

/// Builds a fully-populated `SessionView` for snapshot fixtures.
///
/// Uses an ISO-8601 `started_at` so tree ordering is stable.
#[allow(clippy::too_many_arguments)]
fn make_session(
    id: &str,
    project: &str,
    branch: &str,
    model: &str,
    status: SessionStatus,
    status_label: &str,
    ctx_pct: f64,
    cost: f64,
    started_at: &str,
) -> SessionView {
    SessionView {
        id: SessionId::new(id),
        id_short: id.get(..8).unwrap_or(id).to_string(),
        agent_type: "general".to_string(),
        model: model.to_string(),
        status,
        status_label: status_label.to_string(),
        activity_detail: None,
        should_blink: matches!(status, SessionStatus::AttentionNeeded),
        status_icon: ">".to_string(),
        context_percentage: ctx_pct,
        context_display: format!("{}%", ctx_pct as u32),
        context_warning: ctx_pct >= 70.0,
        context_critical: ctx_pct >= 90.0,
        cost_display: format!("${cost:.2}"),
        cost_usd: cost,
        duration_display: "5m".to_string(),
        duration_seconds: 300.0,
        lines_display: "+100 -20".to_string(),
        working_directory: Some(project.to_string()),
        needs_attention: matches!(status, SessionStatus::AttentionNeeded),
        last_activity_display: "10s ago".to_string(),
        age_display: "5m ago".to_string(),
        started_at: started_at.to_string(),
        last_activity: started_at.to_string(),
        tmux_pane: None,
        project_root: Some(project.to_string()),
        worktree_path: Some(project.to_string()),
        worktree_branch: Some(branch.to_string()),
        ..Default::default()
    }
}

// Intentional UI: only the named parent gets a waiting-decision mark.
fn decision_mark_app() -> App {
    let mut app = App::new();
    let parent = SessionView {
        id: "parent".into(),
        id_short: "parent".into(),
        project_root: Some("/fixtures/project".into()),
        tmux_pane: Some("%7".into()),
        ..Default::default()
    };
    let child = SessionView {
        id: "child".into(),
        id_short: "child".into(),
        parent_session_id: Some(parent.id.clone()),
        ..parent.clone()
    };
    app.update_sessions(vec![parent, child]);
    app.decision_panes.insert("%7".into());
    app
}
#[test]
fn waiting_decision_normal() {
    let app = decision_mark_app();
    let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|f| render_session_list(f, f.area(), &app))
        .unwrap();
    insta::assert_debug_snapshot!(terminal.backend().buffer());
}
#[test]
fn waiting_decision_compact() {
    let app = decision_mark_app();
    let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
    terminal
        .draw(|f| render_compact_session_list(f, f.area(), &app))
        .unwrap();
    insta::assert_debug_snapshot!(terminal.backend().buffer());
}

fn make_app_with_sessions() -> App {
    let mut app = App::new();
    app.state = AppState::Connected;
    app.update_sessions(vec![
        make_session(
            "abc12345-aaaa-bbbb-cccc-000000000001",
            "/home/dev/project-alpha",
            "main",
            "Opus 4.5",
            SessionStatus::Working,
            "working",
            45.0,
            0.50,
            "2026-01-15T10:05:00Z",
        ),
        make_session(
            "def67890-aaaa-bbbb-cccc-000000000002",
            "/home/dev/project-alpha",
            "main",
            "Sonnet 4.5",
            SessionStatus::Idle,
            "idle",
            72.0,
            1.20,
            "2026-01-15T10:00:00Z",
        ),
    ]);
    app
}

fn coordinator_app() -> App {
    let mut app = make_app_with_sessions();
    let mut sessions: Vec<_> = app.sessions.values().cloned().collect();
    for session in &mut sessions {
        session.tmux_pane = Some(
            if session.id_short == "def67890" {
                "%1"
            } else {
                "%0"
            }
            .into(),
        );
        session.worktree_branch = Some("a-long-coordinator-branch".into());
    }
    let lead = sessions
        .iter()
        .find(|s| s.id_short == "def67890")
        .unwrap()
        .clone();
    for (id, pane, parent, time) in [
        ("child000", "%1", lead.id.clone(), "2026-01-15T10:01:00Z"),
        ("mate0000", "%2", lead.id.clone(), "2026-01-15T10:02:00Z"),
        (
            "grand000",
            "%2",
            SessionId::new("mate0000"),
            "2026-01-15T10:03:00Z",
        ),
    ] {
        let mut session = lead.clone();
        session.id = SessionId::new(id);
        session.id_short = id.into();
        session.tmux_pane = Some(pane.into());
        session.parent_session_id = Some(parent);
        session.started_at = time.into();
        session.worktree_branch = None;
        sessions.push(session);
    }
    app.replace_sessions(sessions);
    app.update_coordinator_panes(["%1".to_string(), "%2".to_string()].into());
    app.expand_all();
    app
}

fn assert_coordinator_cells(buf: &Buffer) {
    let tags: Vec<_> = buf
        .content
        .iter()
        .filter(|cell| cell.symbol() == "C")
        .collect();
    assert_eq!(tags.len(), 2, "only lead and separate-pane teammate show C");
    for cell in tags {
        assert_eq!(cell.fg, ratatui::style::Color::Cyan);
        assert!(cell.modifier.contains(ratatui::style::Modifier::BOLD));
    }
}

#[test]
fn session_list_coordinator() {
    let app = coordinator_app();
    let buf = render_buffer(60, 12, |frame, area| render_session_list(frame, area, &app));
    assert_coordinator_cells(&buf);
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn compact_session_list_coordinator() {
    let app = coordinator_app();
    let buf = render_buffer(28, 12, |frame, area| {
        render_compact_session_list(frame, area, &app)
    });
    assert_coordinator_cells(&buf);
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn hidden_parent_rows_do_not_tag_shared_pane_children_in_either_layout() {
    let mut app = coordinator_app();
    app.tree_rows.retain(|row| matches!(&row.kind,
        atm_core::TreeRowKind::Agent { session } if session.id_short == "child000" || session.id_short == "grand000"));
    for compact in [false, true] {
        let buf = render_buffer(60, 12, |frame, area| {
            if compact {
                render_compact_session_list(frame, area, &app);
            } else {
                render_session_list(frame, area, &app);
            }
        });
        assert!(!buf.content.iter().any(|cell| cell.symbol() == "C"));
    }
}

/// Renders into a `TestBackend` and returns a clone of the resulting buffer.
/// Cloning is necessary because `terminal.backend()` borrows the terminal.
fn render_buffer<F>(width: u16, height: u16, draw: F) -> Buffer
where
    F: FnOnce(&mut ratatui::Frame, Rect),
{
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("create terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw(frame, area);
        })
        .expect("draw frame");
    terminal.backend().buffer().clone()
}

// ---- Split layout: session_list -------------------------------------------

#[test]
fn session_list_two_agents_in_one_project() {
    let app = make_app_with_sessions();
    let buf = render_buffer(40, 12, |frame, area| {
        render_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn session_list_attention_needed_row() {
    let mut app = App::new();
    app.state = AppState::Connected;
    app.blink_visible = true;
    app.update_sessions(vec![make_session(
        "att00000-aaaa-bbbb-cccc-000000000003",
        "/home/dev/project-alpha",
        "main",
        "Opus 4.5",
        SessionStatus::AttentionNeeded,
        "attention",
        88.0,
        2.10,
        "2026-01-15T10:10:00Z",
    )]);
    let buf = render_buffer(40, 8, |frame, area| {
        render_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn session_list_multi_project_with_worktrees() {
    let mut app = App::new();
    app.state = AppState::Connected;
    // Two projects, one with two worktrees → exercises the worktree-grouping branch
    app.update_sessions(vec![
        make_session(
            "alpha001-aaaa-bbbb-cccc-000000000001",
            "/home/dev/project-alpha",
            "main",
            "Opus 4.5",
            SessionStatus::Working,
            "working",
            30.0,
            0.10,
            "2026-01-15T10:05:00Z",
        ),
        make_session(
            "alpha002-aaaa-bbbb-cccc-000000000002",
            "/home/dev/project-alpha",
            "feat-x",
            "Opus 4.5",
            SessionStatus::Idle,
            "idle",
            55.0,
            0.20,
            "2026-01-15T10:00:00Z",
        ),
        make_session(
            "beta0001-aaaa-bbbb-cccc-000000000003",
            "/home/dev/project-beta",
            "main",
            "Sonnet 4.5",
            SessionStatus::Idle,
            "idle",
            10.0,
            0.05,
            "2026-01-15T09:55:00Z",
        ),
    ]);
    // Force the worktree grouping by giving the alpha project a 2nd worktree path
    if let Some(s) = app.sessions.values_mut().find(|s| s.id_short == "alpha002") {
        s.worktree_path = Some("/home/dev/project-alpha-feat-x".to_string());
    }
    // rebuild_tree is private — replace_sessions triggers it
    let snapshot: Vec<SessionView> = app.sessions.values().cloned().collect();
    app.replace_sessions(snapshot);

    let buf = render_buffer(45, 16, |frame, area| {
        render_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn session_list_collapsed_with_attention_count() {
    // Collapses all groups so the project row shows ▸ icon plus the
    // agent count badge "(2)" plus the bubbled-up attention "!" marker —
    // a combination that doesn't appear in any expanded-tree snapshot.
    let mut app = App::new();
    app.state = AppState::Connected;
    app.blink_visible = true;
    app.update_sessions(vec![
        make_session(
            "wrk00001-aaaa-bbbb-cccc-000000000001",
            "/home/dev/project-alpha",
            "main",
            "Opus 4.5",
            SessionStatus::Working,
            "working",
            45.0,
            0.50,
            "2026-01-15T10:05:00Z",
        ),
        make_session(
            "att00002-aaaa-bbbb-cccc-000000000002",
            "/home/dev/project-alpha",
            "main",
            "Opus 4.5",
            SessionStatus::AttentionNeeded,
            "attention",
            88.0,
            1.00,
            "2026-01-15T10:00:00Z",
        ),
    ]);
    app.collapse_all();

    let buf = render_buffer(40, 6, |frame, area| {
        render_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn session_list_subagent_nesting() {
    // Parent agent with one nested subagent. Subagents are linked via
    // parent_session_id / child_session_ids and render at depth+1 under
    // their parent in the flattened tree.
    let mut parent = make_session(
        "parent00-aaaa-bbbb-cccc-000000000001",
        "/home/dev/project-alpha",
        "main",
        "Opus 4.5",
        SessionStatus::Working,
        "working",
        40.0,
        0.30,
        "2026-01-15T10:05:00Z",
    );
    let mut child = make_session(
        "childsub-aaaa-bbbb-cccc-000000000002",
        "/home/dev/project-alpha",
        "main",
        "Sonnet 4.5",
        SessionStatus::Working,
        "working",
        15.0,
        0.05,
        "2026-01-15T10:06:00Z",
    );
    child.parent_session_id = Some(parent.id.clone());
    parent.child_session_ids = vec![child.id.clone()];

    let mut app = App::new();
    app.state = AppState::Connected;
    app.update_sessions(vec![parent, child]);

    let buf = render_buffer(45, 8, |frame, area| {
        render_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

/// A daemon on another protocol version rejects every connection.
const VERSION_REJECTION: &str = "Protocol version 1.0 not compatible with server version 2.0";

/// A daemon rejects every connection, giving `reason`.
fn rejected_state(reason: &str) -> AppState {
    AppState::Disconnected {
        since: chrono::DateTime::parse_from_rfc3339("2026-01-15T10:00:00Z")
            .expect("valid rfc3339")
            .with_timezone(&chrono::Utc),
        retry_count: 3,
        reason: Some(TuiError::Rejected(reason.to_string()).to_string()),
    }
}

#[test]
fn session_list_disconnected_rejected() {
    // The reason replaces "Lost connection" and wraps to the list's width.
    let mut app = App::new();
    app.state = rejected_state(VERSION_REJECTION);
    let buf = render_buffer(36, 18, |frame, area| {
        render_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn session_list_connected_empty() {
    // Connected to the daemon, but no agent has reported yet.
    let mut app = App::new();
    app.state = AppState::Connected;
    let buf = render_buffer(64, 16, |frame, area| {
        render_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn compact_session_list_connected_empty() {
    // The sidebar reuses the empty state at as little as 20 columns
    // (SIDEBAR_MIN), where it clips: the steps must still read.
    let mut app = App::new();
    app.state = AppState::Connected;
    let buf = render_buffer(20, 16, |frame, area| {
        render_compact_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

// ---- Split layout: detail_panel -------------------------------------------

/// Frame time for detail-panel snapshots: 10s after the fixtures'
/// `last_activity` (`2026-01-15T10:05:00Z`), so "Activity" reads "10s ago".
fn detail_frame_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-01-15T10:05:10Z")
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn detail_panel_inline_no_capture() {
    let session = make_session(
        "abc12345-aaaa-bbbb-cccc-000000000001",
        "/home/dev/project-alpha",
        "main",
        "Opus 4.5",
        SessionStatus::Working,
        "working",
        45.0,
        0.50,
        "2026-01-15T10:05:00Z",
    );
    let buf = render_buffer(60, 20, |frame, area| {
        render_detail_panel_inline(frame, area, Some(&session), &[], detail_frame_time());
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn detail_panel_inline_empty() {
    let buf = render_buffer(60, 20, |frame, area| {
        render_detail_panel_inline(frame, area, None, &[], detail_frame_time());
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn detail_panel_inline_with_capture_split() {
    // Non-empty captured_output triggers the 40/60 split layout (metadata + terminal)
    let session = make_session(
        "abc12345-aaaa-bbbb-cccc-000000000001",
        "/home/dev/project-alpha",
        "main",
        "Opus 4.5",
        SessionStatus::Working,
        "working",
        45.0,
        0.50,
        "2026-01-15T10:05:00Z",
    );
    let capture = vec![
        "$ cargo test".to_string(),
        "   Compiling atm-tui v0.2.2".to_string(),
        "    Finished in 1.23s".to_string(),
        "running 5 tests".to_string(),
        "test ui::snapshot ... ok".to_string(),
    ];
    let buf = render_buffer(60, 24, |frame, area| {
        render_detail_panel_inline(frame, area, Some(&session), &capture, detail_frame_time());
    });
    insta::assert_debug_snapshot!(buf);
}

// ---- Status bar -----------------------------------------------------------

#[test]
fn header_connected_with_sessions() {
    let app = make_app_with_sessions();
    let buf = render_buffer(80, 3, |frame, area| {
        render_header(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn header_disconnected_with_retries() {
    let mut app = App::new();
    app.state = AppState::Disconnected {
        // Fixed timestamp so snapshot is stable; not displayed but must be set
        since: chrono::DateTime::parse_from_rfc3339("2026-01-15T10:00:00Z")
            .expect("valid rfc3339")
            .with_timezone(&chrono::Utc),
        retry_count: 5,
        reason: None,
    };
    let buf = render_buffer(80, 3, |frame, area| {
        render_header(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn footer_in_tmux() {
    let buf = with_tmux(|| {
        let app = App::new();
        render_buffer(80, 3, |frame, area| {
            render_footer(frame, area, &app);
        })
    });
    insta::assert_debug_snapshot!(buf);
}

// ---- Help popup -----------------------------------------------------------

#[test]
fn help_popup_in_tmux() {
    // Terminal sized so the centered popup (60% × 70% = 72×28) has a
    // ~70-col inner content area — wide enough for the longest help line
    // ("Enter       Jump to session (leaf) / toggle fold (group)") to
    // render in full without clipping.
    let buf = with_tmux(|| {
        render_buffer(120, 40, |frame, area| {
            render_help_popup(frame, area, 0);
        })
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn help_popup_scrolled_to_bottom() {
    // Same size as above; a scroll past the end clamps so the last entry
    // sits just above the bottom border.
    let buf = with_tmux(|| {
        render_buffer(120, 40, |frame, area| {
            render_help_popup(frame, area, u16::MAX);
        })
    });
    insta::assert_debug_snapshot!(buf);
}

// ---- Compact (sidebar) layout --------------------------------------------

#[test]
fn compact_session_list_narrow() {
    let app = make_app_with_sessions();
    let buf = render_buffer(28, 12, |frame, area| {
        render_compact_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn compact_session_list_disconnected_rejected() {
    // Sessions from before the rejection are stale, so the reason shows
    // instead of them, wrapped to the narrowest sidebar (SIDEBAR_MIN).
    let mut app = make_app_with_sessions();
    app.state = rejected_state(VERSION_REJECTION);
    let buf = render_buffer(20, 30, |frame, area| {
        render_compact_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn compact_session_list_subscriber_cap_rejected() {
    // A workspace with more windows than the daemon's subscriber cap: the
    // refused sidebar says why instead of showing a list that never updates.
    let mut app = make_app_with_sessions();
    app.state = rejected_state("Too many subscribers (max: 64)");
    let buf = render_buffer(20, 30, |frame, area| {
        render_compact_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn compact_preview_falls_back_to_status() {
    // No cached Beads task, so the status-line fallback kicks in.
    let session = make_session(
        "abc12345-aaaa-bbbb-cccc-000000000001",
        "/home/dev/project-alpha",
        "main",
        "Opus 4.5",
        SessionStatus::Working,
        "working",
        45.0,
        0.50,
        "2026-01-15T10:05:00Z",
    );
    let buf = render_buffer(30, 8, |frame, area| {
        render_compact_preview(frame, area, Some(&session), &[], None);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn compact_preview_shows_cached_beads_task() {
    // The task comes from App's cache as plain data; the renderer reads no
    // file, so a stray .beads/ on the host can't change the snapshot.
    let session = make_session(
        "abc12345-aaaa-bbbb-cccc-000000000001",
        "/home/dev/project-alpha",
        "main",
        "Opus 4.5",
        SessionStatus::Working,
        "working",
        45.0,
        0.50,
        "2026-01-15T10:05:00Z",
    );
    let task = BeadsTask {
        id: "project-alpha-7kq".to_string(),
        title: "Cache beads reads".to_string(),
        description: Some("Read off the render path.\nKeep the last value.".to_string()),
    };
    let buf = render_buffer(30, 8, |frame, area| {
        render_compact_preview(frame, area, Some(&session), &[], Some(&task));
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn compact_footer_default() {
    let app = App::new();
    let buf = render_buffer(30, 1, |frame, area| {
        render_compact_footer(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

// ---- Filter (`/` search) ---------------------------------------------------

/// Lead (Opus) with a Sonnet teammate, plus an unrelated Opus session,
/// filtered by "sonnet": the lead stays as dimmed context, the other hides.
fn make_filtered_app() -> App {
    let lead = make_session(
        "lead0000-aaaa-bbbb-cccc-000000000001",
        "/home/dev/project-alpha",
        "main",
        "Opus 4.5",
        SessionStatus::Idle,
        "idle",
        40.0,
        0.30,
        "2026-01-15T10:05:00Z",
    );
    let mut mate = make_session(
        "mate0000-aaaa-bbbb-cccc-000000000002",
        "/home/dev/project-alpha",
        "main",
        "Sonnet 4.5",
        SessionStatus::Working,
        "working",
        15.0,
        0.05,
        "2026-01-15T10:06:00Z",
    );
    mate.parent_session_id = Some(lead.id.clone());
    let other = make_session(
        "other000-aaaa-bbbb-cccc-000000000003",
        "/home/dev/project-alpha",
        "main",
        "Opus 4.5",
        SessionStatus::Idle,
        "idle",
        10.0,
        0.10,
        "2026-01-15T10:07:00Z",
    );
    let mut app = App::new();
    app.state = AppState::Connected;
    app.update_sessions(vec![lead, mate, other]);
    app.start_search();
    for c in "sonnet".chars() {
        app.push_search_char(c);
    }
    app
}

#[test]
fn session_list_filter_dims_context_lead() {
    let app = make_filtered_app();
    let buf = render_buffer(45, 6, |frame, area| {
        render_session_list(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn footer_with_active_filter_80_cols() {
    // After Enter closes the prompt, the filter must stay visible at a
    // normal width; it leads the footer so hints clip instead.
    let buf = with_tmux(|| {
        let mut app = make_filtered_app();
        app.confirm_search();
        render_buffer(80, 3, |frame, area| {
            render_footer(frame, area, &app);
        })
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn footer_search_prompt() {
    let app = make_filtered_app();
    let buf = render_buffer(50, 3, |frame, area| {
        render_footer(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}

fn work_windows(other_window_child: bool) -> WorkWindows {
    serde_json::from_value(serde_json::json!({"state":"ok","windows":[
        {"id":"@3","name":"support","links":[{"session":"work","index":3}],"panes":["%7","%8"]},
        {"id":"@4","name":"review","links":[{"session":"work","index":4}],"panes": if other_window_child {vec!["%9","%10"]} else {vec!["%10"]}}
    ]})).unwrap()
}
fn work_org() -> WorkOrg {
    serde_json::from_value(serde_json::json!({"seq":41,"projects":[{
        "seats":[{"agent_id":"alpha/watcher","pane":"%7","tasks":["t41","t42"]},
                 {"agent_id":"alpha/reviewer","pane":"%10","tasks":[]}],
        "tasks":[{"task_id":"t41","title":"Watch the login"},{"task_id":"t42","title":"Check docs"}]
    }],"needs_owner":{"items":[{"kind":"decision_pending","agent_id":"alpha/watcher"},
        {"kind":"seat_attention","agent_id":"alpha/watcher"}]}}))
    .unwrap()
}
fn work_load(app: &mut App, effects: Vec<WorkEffect>, windows: WorkWindows, org: WorkOrg) {
    for effect in effects {
        let input = match effect {
            WorkEffect::RequestWindows(generation) => WorkInput::WindowsReply {
                generation,
                result: Ok(windows.clone()),
            },
            WorkEffect::RequestOrg(generation) => WorkInput::OrgReply {
                generation,
                result: Ok(org.clone()),
            },
            _ => continue,
        };
        let _ = app.reduce_work(input, Duration::ZERO);
    }
}
fn work_app() -> App {
    let mut app = App::new();
    let mut parent = make_session(
        "parent01",
        "/fixtures/alpha",
        "cp/work-a",
        "Opus 4.5",
        SessionStatus::Idle,
        "idle",
        12.0,
        0.0,
        "2026-01-15T12:00:00Z",
    );
    parent.tmux_pane = Some("%7".into());
    let child = SessionView {
        id: "child001".into(),
        id_short: "child001".into(),
        parent_session_id: Some(parent.id.clone()),
        worktree_branch: None,
        status: SessionStatus::Working,
        context_percentage: 0.0,
        ..parent.clone()
    };
    let mut other = make_session(
        "unnamed1",
        "/fixtures/beta",
        "cp/u2",
        "Model B",
        SessionStatus::Working,
        "working",
        54.0,
        0.0,
        "2026-01-15T12:00:01Z",
    );
    other.tmux_pane = Some("%8".into());
    other.harness = "pi".into();
    let mut review = make_session(
        "review01",
        "/fixtures/alpha",
        "cp/review",
        "Model C",
        SessionStatus::Idle,
        "idle",
        0.0,
        0.0,
        "2026-01-15T12:00:02Z",
    );
    review.tmux_pane = Some("%10".into());
    review.harness = "codex".into();
    app.update_sessions(vec![parent, child, other, review]);
    let _ = app.reduce_work(
        WorkInput::Capabilities {
            methods: vec!["session.windows".into(), "org.tree".into()],
            watch: false,
        },
        Duration::ZERO,
    );
    let effects = app.toggle_work_view(Duration::ZERO);
    work_load(&mut app, effects, work_windows(false), work_org());
    app.selected_index = app.tree_rows.iter().position(|row| matches!(&row.kind, atm_core::TreeRowKind::Agent {session} if session.id.as_str() == "parent01")).unwrap();
    app
}
fn work_buffer(app: &App, compact: bool) -> Buffer {
    render_buffer(if compact { 40 } else { 100 }, 12, |frame, area| {
        if compact {
            render_compact_session_list(frame, area, app);
        } else {
            render_session_list(frame, area, app);
        }
    })
}
fn buffer_text(buffer: &Buffer) -> String {
    buffer
        .content
        .chunks(usize::from(buffer.area.width).max(1))
        .map(|row| {
            let mut text = String::new();
            let mut covered = 0;
            for cell in row {
                if covered > 0 {
                    covered -= 1;
                    continue;
                }
                text.push_str(cell.symbol());
                covered = ratatui::text::Span::raw(cell.symbol())
                    .width()
                    .saturating_sub(1);
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn work_windows_with_marks() {
    let buf = work_buffer(&work_app(), false);
    let text = buffer_text(&buf);
    assert!(
        text.contains("Work · 2 windows, 4 agents")
            && text.contains("2 open")
            && text.contains("?1")
    );
    assert!(
        text.contains("watcher")
            && text.contains("◐ t41 Watch the login +1")
            && text.contains("cp/work-a")
    );
    insta::assert_debug_snapshot!(buf);
}
#[test]
fn work_folded_window_counts() {
    let mut app = work_app();
    app.collapse_all();
    let buf = work_buffer(&app, false);
    let text = buffer_text(&buf);
    assert!(text.contains("support (3)") && text.contains("review (1)") && text.contains("2 open"));
    insta::assert_debug_snapshot!(buf);
}
#[test]
fn work_no_window_group() {
    let mut app = work_app();
    let effects = app.reduce_work(WorkInput::Refresh, Duration::ZERO);
    work_load(
        &mut app,
        effects,
        serde_json::from_value(serde_json::json!({"state":"ok","windows":[]})).unwrap(),
        work_org(),
    );
    let buf = work_buffer(&app, false);
    assert!(
        buffer_text(&buf).contains("Work · 0 windows, 4 agents")
            && buffer_text(&buf).contains("(no window)")
    );
    insta::assert_debug_snapshot!(buf);
}
#[test]
fn work_child_in_other_window() {
    let mut app = work_app();
    let mut child = app
        .sessions
        .values()
        .find(|s| s.id.as_str() == "child001")
        .unwrap()
        .clone();
    child.tmux_pane = Some("%9".into());
    app.update_sessions(vec![child]);
    let effects = app.reduce_work(WorkInput::Refresh, Duration::ZERO);
    work_load(&mut app, effects, work_windows(true), work_org());
    let buf = work_buffer(&app, false);
    let text = buffer_text(&buf);
    assert!(text.find("4 review").unwrap() < text.find("child001").unwrap());
    insta::assert_debug_snapshot!(buf);
}
#[test]
fn work_no_org_data() {
    let mut app = work_app();
    let _ = app.reduce_work(
        WorkInput::Capabilities {
            methods: vec!["session.windows".into()],
            watch: false,
        },
        Duration::ZERO,
    );
    let buf = work_buffer(&app, false);
    let text = buffer_text(&buf);
    assert!(text.contains("no org data") && !text.contains("watcher") && !text.contains("t41"));
    insta::assert_debug_snapshot!(buf);
}
#[test]
fn work_stale() {
    let mut app = work_app();
    app.work_now = Duration::from_secs(12);
    let buf = work_buffer(&app, false);
    assert!(buffer_text(&buf).contains("stale 12s"));
    insta::assert_debug_snapshot!(buf);
}
#[test]
fn work_needs_newer_daemon() {
    let mut app = work_app();
    let _ = app.reduce_work(
        WorkInput::Capabilities {
            methods: vec!["org.tree".into()],
            watch: true,
        },
        Duration::ZERO,
    );
    let buf = work_buffer(&app, false);
    assert!(buffer_text(&buf).contains("Work needs a newer atmd. w: Sessions"));
    insta::assert_debug_snapshot!(buf);
}
#[test]
fn compact_work_list() {
    let buf = work_buffer(&work_app(), true);
    assert!(buffer_text(&buf).contains("watcher") && buffer_text(&buf).contains("2 open"));
    insta::assert_debug_snapshot!(buf);
}

// Lead Medium 1: a pane-sharing child must never impersonate the parent seat.
#[test]
fn work_pane_marks_belong_only_to_parentless_rows() {
    let app = work_app();
    for compact in [false, true] {
        let text = buffer_text(&work_buffer(&app, compact));
        let parent = text.lines().find(|line| line.contains("watcher")).unwrap();
        assert!(
            parent.contains("t41") && parent.contains('?') && parent.contains('!'),
            "{parent}"
        );
        let child = text.lines().find(|line| line.contains("child001")).unwrap();
        assert!(
            !child.contains("watcher")
                && !child.contains("t41")
                && !child.contains('?')
                && !child.contains('!'),
            "{child}"
        );
    }
}

// Break caught: missing titles invent text/counts, or new Work text introduces controls into rows.
#[test]
fn work_capped_titles_and_untrusted_unicode_text_remain_honest() {
    let mut app = work_app();
    let mut org = work_org();
    org.projects.first_mut().unwrap().tasks.clear();
    let effects = app.reduce_work(WorkInput::Refresh, Duration::ZERO);
    work_load(&mut app, effects, work_windows(false), org);
    let text = buffer_text(&work_buffer(&app, false));
    assert!(
        text.contains("2 open") && text.contains("◐ t41 +1") && !text.contains("Watch the login")
    );
    let mut org = work_org();
    org.projects
        .first_mut()
        .unwrap()
        .tasks
        .first_mut()
        .unwrap()
        .title = "界\u{1b}[2J\tA\nB".repeat(40);
    let mut windows = work_windows(false);
    windows.windows.first_mut().unwrap().name = "支援\u{1b}[2J\nname".into();
    let effects = app.reduce_work(WorkInput::Refresh, Duration::ZERO);
    work_load(&mut app, effects, windows, org);
    for width in [0, 1, 20, 30, 100] {
        for compact in [false, true] {
            let buf = render_buffer(width, 12, |frame, area| {
                if compact {
                    render_compact_session_list(frame, area, &app)
                } else {
                    render_session_list(frame, area, &app)
                }
            });
            assert!(buf
                .content
                .iter()
                .all(|cell| !cell.symbol().chars().any(char::is_control)));
        }
    }
    let text = buffer_text(&work_buffer(&app, false));
    assert!(
        text.contains("支援\\u{1b}[2J\\nname") && text.contains("界\\u{1b}[2J\\tA\\nB"),
        "{text}"
    );
}

// Break caught: Work counts visible rows instead of full filtered groups, or old-daemon state leaks into Sessions.
#[test]
fn work_counts_empty_filter_and_capabilities_leave_sessions_unchanged() {
    let mut app = work_app();
    let _ = app.toggle_work_view(Duration::ZERO);
    let sessions = work_buffer(&app, false);
    let _ = app.reduce_work(
        WorkInput::Capabilities {
            methods: vec![],
            watch: false,
        },
        Duration::ZERO,
    );
    let _ = app.toggle_work_view(Duration::ZERO);
    assert!(buffer_text(&work_buffer(&app, false)).contains("needs a newer atmd"));
    let _ = app.toggle_work_view(Duration::ZERO);
    assert_eq!(work_buffer(&app, false), sessions);
    let mut app = work_app();
    app.start_search();
    for c in "pi".chars() {
        app.push_search_char(c);
    }
    app.collapse_all();
    assert!(buffer_text(&work_buffer(&app, false)).contains("Work · 1 windows, 1 agents"));
    app.clear_filter();
    app.replace_sessions(vec![]);
    assert!(buffer_text(&work_buffer(&app, false)).contains("Work · 0 windows, 0 agents"));
}

#[test]
fn compact_footer_with_active_filter() {
    let mut app = make_filtered_app();
    app.confirm_search();
    let buf = render_buffer(30, 1, |frame, area| {
        render_compact_footer(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}
