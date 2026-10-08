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

use std::sync::Mutex;

use atm_core::{SessionId, SessionStatus, SessionView};
use atm_tui::app::{App, AppState};
use atm_tui::ui::{
    detail_panel::{render_compact_preview, render_detail_panel_inline},
    help_popup::render_help_popup,
    session_list::{render_compact_session_list, render_session_list},
    status_bar::{render_compact_footer, render_footer, render_header},
};
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

// ---- Split layout: detail_panel -------------------------------------------

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
        render_detail_panel_inline(frame, area, Some(&session), &[]);
    });
    insta::assert_debug_snapshot!(buf);
}

#[test]
fn detail_panel_inline_empty() {
    let buf = render_buffer(60, 20, |frame, area| {
        render_detail_panel_inline(frame, area, None, &[]);
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
        render_detail_panel_inline(frame, area, Some(&session), &capture);
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
fn compact_preview_falls_back_to_status() {
    // Both project_root and worktree_path are None, so the renderer's
    // beads lookup short-circuits before touching the filesystem and the
    // status-line fallback kicks in. Strictly deterministic — no probe,
    // no chance of a stray .beads/ on the dev/CI host changing the snapshot.
    let mut session = make_session(
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
    session.project_root = None;
    session.worktree_path = None;
    let buf = render_buffer(30, 8, |frame, area| {
        render_compact_preview(frame, area, Some(&session), &[]);
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

#[test]
fn compact_footer_with_active_filter() {
    let mut app = make_filtered_app();
    app.confirm_search();
    let buf = render_buffer(30, 1, |frame, area| {
        render_compact_footer(frame, area, &app);
    });
    insta::assert_debug_snapshot!(buf);
}
