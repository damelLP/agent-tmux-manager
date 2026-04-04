# Compact Sidebar TUI — Implementation Plan

## Overview

Add a `--compact` flag that switches the ATM TUI from the existing full-width horizontal split (30% list / 70% detail) to a vertical layout optimized for 20-40 column sidebar panes.

## Target Layout

```
┌─ ATM · ● · 5 ──────────┐  header (existing)
│ ▼ my-project      (3)  │
│   > 45% my-feature      │  ~70% height
│   > 12% refactor        │  tree list (full width)
│   - idle-agent          │
│                         │
│ ▼ other-repo       (1)  │
│   > 78% bugfix          │
├─ Task ──────────────────┤
│ ▸ Fix sidebar #5  [3/7] │  task context (1-2 lines)
├─ Terminal ──────────────┤
│ $ cargo test            │  ~30% height
│   Running 12 tests      │  live tmux capture
│   test parse ... ok     │
│   test render ... ok    │
├─────────────────────────┤
│ ? help                  │  1-line footer
└─────────────────────────┘
```

## Tasks

### Task 1: Add `--compact` flag and `compact` field on App

**Files:** `crates/atm/src/main.rs`, `crates/atm/src/app.rs`

**main.rs — Args struct** (line ~69, after `tmux_session`):
```rust
/// Compact sidebar mode: vertical layout for narrow panes
#[arg(long)]
compact: bool,
```

**app.rs — App struct** (line ~101, after `filter_pane_ids`):
```rust
/// Compact mode: vertical layout optimized for narrow sidebar panes.
pub compact: bool,
```

**app.rs — App::new()**: Initialize `compact: false`.

**main.rs — app initialization** (line ~684): Pass `args.compact` to set `app.compact = true`.

**Verification:** `cargo build --bin atm-tui` compiles. `atm --compact --help` shows the flag.

---

### Task 2: Add `CompactLayout` to layout.rs

**File:** `crates/atm/src/ui/layout.rs`

Add a new layout struct alongside existing `AppLayout`:

```rust
/// Compact sidebar layout — vertical split, no detail side panel.
///
/// ```text
/// ┌─ Header ────────────┐  3 lines
/// │ Tree list           │  ~70% of remaining
/// ├─ Preview ───────────┤  ~30% of remaining
/// │ (task + capture)    │
/// ├─ Footer ────────────┤  1 line
/// └─────────────────────┘
/// ```
#[derive(Debug, Clone, Copy)]
pub struct CompactLayout {
    pub header: Rect,
    pub list_area: Rect,       // full width, ~70% of content height
    pub preview_area: Rect,    // full width, ~30% of content height
    pub footer: Rect,
}

impl CompactLayout {
    pub fn new(area: Rect) -> Self {
        let [header, content, footer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),  // Header
                Constraint::Min(6),    // Content (minimum 6 lines)
                Constraint::Length(1), // Footer (compact: just "? help")
            ])
            .areas(area);

        let [list_area, preview_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(70), // Tree list
                Constraint::Percentage(30), // Preview (task + capture)
            ])
            .areas(content);

        Self { header, list_area, preview_area, footer }
    }
}
```

**Tests:** Add `test_compact_layout_creation` mirroring existing `test_app_layout_creation`.

---

### Task 3: Compact agent row renderer

**File:** `crates/atm/src/ui/session_list.rs`

Add `create_compact_agent_line()` alongside existing `create_agent_line()`:

```rust
/// Creates a compact agent line: icon + context% + display name.
/// Name is resolved: worktree_branch → session label → id_short.
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

    // Name resolution: branch → id_short
    let name = session.worktree_branch.as_deref()
        .unwrap_or(&session.id_short);

    // Calculate available space for name:
    // 1 (selector) + indent + 2 (icon+space) + 5 (ctx%) + 1 (space) = 9 + indent
    let overhead = 9 + indent.len() as u16;
    let name_width = available_width.saturating_sub(overhead) as usize;
    let truncated_name = truncate_string(name, name_width);

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
```

Add `render_compact_session_list()` that calls `create_compact_agent_line` for Agent rows and the existing `create_group_line` for group rows. Signature:

```rust
pub fn render_compact_session_list(frame: &mut Frame, area: Rect, app: &App) {
    // Same as render_session_list but uses create_compact_agent_line
    // and passes area.width to each row for adaptive truncation
}
```

**Verification:** Write a test with `TestBackend::new(30, 40)` to confirm rendering at narrow widths.

---

### Task 4: Compact preview panel renderer

**File:** `crates/atm/src/ui/detail_panel.rs` (or new file `crates/atm/src/ui/compact_preview.rs`)

Create `render_compact_preview()`:

```rust
/// Renders the compact preview pane (task context + terminal capture).
pub fn render_compact_preview(
    frame: &mut Frame,
    area: Rect,
    session: Option<&SessionView>,
    captured_output: &[String],
) {
    // Split: 2 lines for task context, rest for terminal capture
    let [task_area, capture_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),  // Task context
            Constraint::Min(3),    // Terminal capture
        ])
        .areas(area);

    // Task context — placeholder for now, beads integration later
    render_task_context(frame, task_area, session);

    // Terminal capture — reuse existing auto-scroll logic
    render_terminal_capture(frame, capture_area, captured_output);
}
```

`render_task_context` is a stub for now showing session status + model in 2 lines (the beads `TaskContextProvider` integration is a follow-on). Display:
```
Line 1: status_label (activity_detail)
Line 2: model · cost · context%
```

`render_terminal_capture` extracts the bottom-of-screen capture logic from existing `render_detail_panel_inline` into a shared helper.

---

### Task 5: Compact footer renderer

**File:** `crates/atm/src/ui/status_bar.rs`

Add `render_compact_footer()`:

```rust
pub fn render_compact_footer(frame: &mut Frame, area: Rect, app: &App) {
    // Single line: "? help" (or "[pick]" indicator in pick mode)
    let text = if app.pick_mode {
        "? help [pick]"
    } else {
        "? help"
    };
    let paragraph = Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(paragraph, area);
}
```

---

### Task 6: Wire compact rendering path in ui/mod.rs

**File:** `crates/atm/src/ui/mod.rs`

Add `render_compact()` alongside existing `render()`:

```rust
/// Renders the compact sidebar TUI layout.
pub fn render_compact(frame: &mut Frame, app: &App) {
    let layout = CompactLayout::new(frame.area());

    render_header(frame, layout.header, app);
    render_compact_footer(frame, layout.footer, app);

    render_compact_session_list(frame, layout.list_area, app);
    render_compact_preview(
        frame,
        layout.preview_area,
        app.selected_session(),
        &app.captured_output,
    );

    if app.show_help {
        help_popup::render_help_popup(frame, frame.area());
    }
}
```

**File:** `crates/atm/src/main.rs` — event loop render call (line ~354):

```rust
terminal.draw(|frame| {
    if app.compact {
        let layout = ui::layout::CompactLayout::new(frame.area());
        viewport_height = layout.list_area.height.saturating_sub(2);
        ui::render_compact(frame, app);
    } else {
        let layout = ui::layout::AppLayout::new(frame.area());
        viewport_height = layout.list_area.height.saturating_sub(2);
        ui::render(frame, app);
    }
})?;
```

---

### Task 7: Wire `--compact` in workspace command

**File:** `src/bin/atm.rs` — `cmd_workspace` function

Update the ATM TUI launch command (currently line ~1311) to pass `--compact`:

```rust
let atm_cmd = format!("atm --compact --tmux-session '{session_name}'");
```

This ensures the workspace sidebar always uses the compact layout.

---

### Task 8: Tests

**Files:** `crates/atm/src/ui/layout.rs`, `crates/atm/src/ui/mod.rs`

1. **Layout test:** `CompactLayout::new()` at various sizes (30x40, 20x24, 40x60)
2. **Render test:** `render_compact()` with `TestBackend::new(30, 40)` — no panics, renders tree
3. **Render test:** `render_compact()` at minimum size `TestBackend::new(20, 12)` — no panics
4. **Agent line test:** Verify `create_compact_agent_line` truncates name correctly at various widths

---

## Build Sequence

```
Task 1 (flag + field)
  └→ Task 2 (CompactLayout)
       └→ Task 3 (compact agent rows)    ─┐
           Task 4 (compact preview panel)  ├→ Task 6 (wire render_compact)
           Task 5 (compact footer)        ─┘      └→ Task 7 (workspace flag)
                                                        └→ Task 8 (tests)
```

Tasks 3, 4, 5 are independent and can be done in parallel.

## Out of Scope (follow-on)

- **Beads `TaskContextProvider`** — task context zone shows session metadata for now
- **Auto-detect compact mode** based on terminal width (< 50 cols)
- **Toggle keybinding** (e.g., Tab) to switch between full and compact at runtime
