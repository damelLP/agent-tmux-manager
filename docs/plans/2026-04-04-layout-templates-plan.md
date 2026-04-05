# ATM v2 Layout Templates — Implementation Plan

**Date:** 2026-04-04
**Beads:** agent-tmux-monitor-557
**Status:** Ready for implementation

## Overview

Add layout template system to ATM: TOML config parsing, 4 built-in presets, layout application via tmux splits, live pane capture in TUI detail panel, tmux-bindings.conf, and `atm layout` CLI subcommand.

---

## Task 1: Add `toml` workspace dependency

**Files to modify:**
- `/home/damel/git/agent-tmux-manager/Cargo.toml` — add `toml = "0.8"` to `[workspace.dependencies]` and `[dependencies]`
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/Cargo.toml` — add `toml = { workspace = true }` to `[dependencies]`

**What to do:**

In root `Cargo.toml`, add to `[workspace.dependencies]` (after the `serde_json` line):
```toml
toml = "0.8"
```

In root `Cargo.toml`, add to `[dependencies]`:
```toml
toml = { workspace = true }
```

In `crates/atm-tmux/Cargo.toml`, add to `[dependencies]`:
```toml
toml = { workspace = true }
```

**Verify:** `cargo check -p atm-tmux` compiles.

---

## Task 2: Layout types and TOML parsing

**Files to create:**
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/src/layout.rs`

**Files to modify:**
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/src/lib.rs` — add `pub mod layout;`

**What to do:**

Create `layout.rs` in `atm-tmux` with these types:

```rust
use std::collections::HashMap;
use std::path::Path;
use serde::Deserialize;
use crate::TmuxError;

/// A tmux layout template — a tree of slots that ATM materializes into panes.
#[derive(Debug, Clone, Deserialize)]
pub struct Layout {
    pub name: String,
    pub root: Slot,
}

/// A single slot in the layout tree.
#[derive(Debug, Clone, Deserialize)]
pub struct Slot {
    pub role: SlotRole,
    /// Size as a percentage string, e.g., "75%", "30%".
    pub size: String,
    pub direction: SplitDirection,
    #[serde(default)]
    pub children: Vec<Slot>,
    /// For agent slots: how many agents to spawn initially.
    #[serde(default = "default_count")]
    pub count: u8,
}

fn default_count() -> u8 { 1 }

/// What purpose a slot serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotRole {
    Agent,
    Editor,
    Shell,
    AtmPanel,
}

/// Direction to split a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Where to materialize a layout.
#[derive(Debug, Clone)]
pub enum LayoutTarget {
    /// Split the specified pane in-place.
    CurrentPane(String),
    /// Create a new tmux window (optional name).
    NewWindow(Option<String>),
    /// Create a new tmux session with the given name.
    NewSession(String),
}

/// Result of applying a layout — maps slot roles to created pane IDs.
#[derive(Debug, Clone)]
pub struct LayoutResult {
    pub panes: HashMap<SlotRole, Vec<String>>,
}
```

Add a `parse_layout` function:

```rust
/// Parses a Layout from a TOML string.
///
/// The TOML should have a top-level `[layout]` table containing `name` and `root`.
pub fn parse_layout(toml_str: &str) -> Result<Layout, LayoutConfigError> {
    #[derive(Deserialize)]
    struct Wrapper {
        layout: Layout,
    }
    let wrapper: Wrapper = toml::from_str(toml_str)
        .map_err(|e| LayoutConfigError::Parse(e.to_string()))?;
    Ok(wrapper.layout)
}

/// Errors from layout configuration.
#[derive(Debug, thiserror::Error)]
pub enum LayoutConfigError {
    #[error("failed to parse layout TOML: {0}")]
    Parse(String),
    #[error("layout not found: {0}")]
    NotFound(String),
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
}
```

Add unit tests:
- Parse the example TOML from the design doc and verify the resulting tree structure
- Parse a minimal layout (single agent slot, no children)
- Parse with missing optional fields (count defaults to 1, children defaults to empty)
- Parse invalid TOML returns error

In `lib.rs`, add after the existing `pub mod mock;` line:
```rust
pub mod layout;
```

**Verify:** `cargo test -p atm-tmux` passes.

---

## Task 3: Built-in preset layouts

**Files to modify:**
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/src/layout.rs` — add preset functions

**What to do:**

Add these functions to `layout.rs`:

```rust
/// Returns the built-in "solo" layout: one full-width agent pane.
/// ATM monitoring via popup overlay.
pub fn preset_solo() -> Layout {
    Layout {
        name: "solo".to_string(),
        root: Slot {
            role: SlotRole::Agent,
            size: "100%".to_string(),
            direction: SplitDirection::Horizontal,
            children: vec![],
            count: 1,
        },
    }
}

/// Returns the built-in "pair" layout:
/// 75% left with 2 agent panes stacked, 25% right ATM sidebar.
pub fn preset_pair() -> Layout {
    Layout {
        name: "pair".to_string(),
        root: Slot {
            role: SlotRole::Shell, // container, not directly used
            size: "100%".to_string(),
            direction: SplitDirection::Horizontal,
            children: vec![
                Slot {
                    role: SlotRole::Shell, // agent container
                    size: "75%".to_string(),
                    direction: SplitDirection::Vertical,
                    children: vec![
                        Slot {
                            role: SlotRole::Agent,
                            size: "50%".to_string(),
                            direction: SplitDirection::Horizontal,
                            children: vec![],
                            count: 1,
                        },
                        Slot {
                            role: SlotRole::Agent,
                            size: "50%".to_string(),
                            direction: SplitDirection::Horizontal,
                            children: vec![],
                            count: 1,
                        },
                    ],
                    count: 1,
                },
                Slot {
                    role: SlotRole::AtmPanel,
                    size: "25%".to_string(),
                    direction: SplitDirection::Vertical,
                    children: vec![],
                    count: 1,
                },
            ],
            count: 1,
        },
    }
}

/// Returns the built-in "squad" layout:
/// 75% left with 3 agent panes (2 top, 1 bottom), 25% right ATM sidebar.
pub fn preset_squad() -> Layout { /* similar structure, 3 agents */ }

/// Returns the built-in "grid" layout:
/// 2x2 grid of 4 agent panes. ATM via popup.
pub fn preset_grid() -> Layout { /* 2x2 grid */ }

/// Returns a preset layout by name, or None if not found.
pub fn preset_by_name(name: &str) -> Option<Layout> {
    match name {
        "solo" => Some(preset_solo()),
        "pair" => Some(preset_pair()),
        "squad" => Some(preset_squad()),
        "grid" => Some(preset_grid()),
        _ => None,
    }
}
```

Add unit tests:
- Each preset returns a valid Layout with the expected name
- `preset_by_name` returns `Some` for known names, `None` for unknown
- Verify agent slot counts match expected values (solo=1, pair=2, squad=3, grid=4)

**Verify:** `cargo test -p atm-tmux` passes.

---

## Task 4: Layout engine — `apply_layout`

**Files to modify:**
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/src/layout.rs` — add `apply_layout` function
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/src/lib.rs` — add `new_session` to `TmuxClient` trait
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/src/client.rs` — implement `new_session` on `RealTmuxClient`
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/src/mock.rs` — implement `new_session` on `MockTmuxClient`

**What to do:**

First, add `new_session` to the `TmuxClient` trait in `lib.rs` (needed for `LayoutTarget::NewSession`):
```rust
/// Creates a new tmux session, returning the initial pane ID.
async fn new_session(&self, name: &str) -> Result<String, TmuxError>;
```

Implement in `client.rs`:
```rust
async fn new_session(&self, name: &str) -> Result<String, TmuxError> {
    let output = self.run("new-session", &["-d", "-s", name, "-P", "-F", "#{pane_id}"]).await?;
    let pane_id = output.trim().to_string();
    if pane_id.is_empty() {
        return Err(TmuxError::ParseError("new-session returned empty pane ID".to_string()));
    }
    Ok(pane_id)
}
```

Add `NewSession` variant to `MockCall` in `mock.rs` and implement on `MockTmuxClient`.

Then add the layout engine in `layout.rs`:

```rust
/// Applies a layout template by creating tmux panes according to the slot tree.
///
/// Returns a `LayoutResult` mapping slot roles to their created pane IDs.
pub async fn apply_layout(
    client: &dyn TmuxClient,
    layout: &Layout,
    target: LayoutTarget,
) -> Result<LayoutResult, TmuxError> {
    // Step 1: Get the root pane ID based on target
    let root_pane = match target {
        LayoutTarget::CurrentPane(ref pane_id) => pane_id.clone(),
        LayoutTarget::NewWindow(ref name) => {
            // Determine session from environment or default
            let session = std::env::var("TMUX_SESSION")
                .unwrap_or_else(|_| "0".to_string());
            client.new_window(&session, None).await?
        }
        LayoutTarget::NewSession(ref name) => {
            client.new_session(name).await?
        }
    };

    // Step 2: Recursively split the root pane according to the layout tree
    let mut result = LayoutResult { panes: HashMap::new() };
    apply_slot(client, &layout.root, &root_pane, &mut result).await?;
    Ok(result)
}

/// Recursively applies a slot, splitting the parent pane for each child.
async fn apply_slot(
    client: &dyn TmuxClient,
    slot: &Slot,
    pane_id: &str,
    result: &mut LayoutResult,
) -> Result<(), TmuxError> {
    if slot.children.is_empty() {
        // Leaf node — record this pane under its role
        result.panes.entry(slot.role).or_default().push(pane_id.to_string());
        return Ok(());
    }

    // Container node — split the pane for each child after the first.
    // First child inherits the current pane. Each subsequent child is split off.
    let mut current_pane = pane_id.to_string();
    for (i, child) in slot.children.iter().enumerate() {
        if i == 0 {
            // First child uses the existing pane
            // Recurse into it using a Box::pin for the recursive async call
            apply_slot(client, child, &current_pane, result).await?;
        } else {
            // Split off a new pane from the parent (not current_pane)
            let horizontal = matches!(slot.direction, SplitDirection::Vertical);
            let new_pane = client.split_window(
                pane_id,
                &child.size,
                horizontal,
                None,
            ).await?;
            apply_slot(client, child, &new_pane, result).await?;
        }
    }
    Ok(())
}
```

Note: The recursive async calls require either `#[async_recursion]` or manual `Box::pin`. Add `async-recursion = "1"` to atm-tmux deps, or use the boxed-future pattern. The simpler approach is `async_recursion`:

Add to `crates/atm-tmux/Cargo.toml`:
```toml
async-recursion = "1"
```

And annotate `apply_slot` with `#[async_recursion::async_recursion(?Send)]` — but since `TmuxClient` requires `Send + Sync`, use `#[async_recursion::async_recursion]`.

Actually, since `&dyn TmuxClient` is not `Send`, use a manual Box::pin approach or make `apply_slot` take `&(dyn TmuxClient + Send + Sync)` explicitly. The cleanest approach: make `apply_slot` a regular function returning `BoxFuture`.

Add unit tests with `MockTmuxClient`:
- `apply_layout` with solo preset → no splits, one pane recorded as Agent
- `apply_layout` with pair preset → verify correct split sequence (horizontal split first, then vertical split for agents)
- `apply_layout` with `NewWindow` target → verify `new_window` called first
- `apply_layout` with `NewSession` target → verify `new_session` called first
- Verify `LayoutResult` contains correct role→pane mappings

**Verify:** `cargo test -p atm-tmux` passes.

---

## Task 5: Config file loading

**Files to modify:**
- `/home/damel/git/agent-tmux-manager/crates/atm-tmux/src/layout.rs` — add `load_layout` function

**What to do:**

```rust
/// Loads a layout by name, checking (in order):
/// 1. Project-local `.atm/layout.toml`
/// 2. Global `~/.config/atm/config.toml`
/// 3. Built-in presets
pub fn load_layout(
    name: &str,
    project_root: Option<&Path>,
) -> Result<Layout, LayoutConfigError> {
    // 1. Check project-local config
    if let Some(root) = project_root {
        let project_config = root.join(".atm/layout.toml");
        if project_config.exists() {
            let content = std::fs::read_to_string(&project_config)?;
            let layout = parse_layout(&content)?;
            if layout.name == name {
                return Ok(layout);
            }
        }
    }

    // 2. Check global config
    if let Some(config_dir) = dirs::config_dir() {
        let global_config = config_dir.join("atm/config.toml");
        if global_config.exists() {
            let content = std::fs::read_to_string(&global_config)?;
            if let Ok(layout) = parse_layout(&content) {
                if layout.name == name {
                    return Ok(layout);
                }
            }
        }
    }

    // 3. Check built-in presets
    preset_by_name(name).ok_or_else(|| LayoutConfigError::NotFound(name.to_string()))
}
```

Add `dirs = "5.0"` to `crates/atm-tmux/Cargo.toml` (already in root workspace deps — add to workspace if not there, or use the existing one).

Add unit tests:
- `load_layout("solo", None)` returns the solo preset
- `load_layout("nonexistent", None)` returns `NotFound` error
- Test with a temp dir containing `.atm/layout.toml` — verify project config overrides preset

**Verify:** `cargo test -p atm-tmux` passes.

---

## Task 6: Live pane capture in TUI detail panel

**Files to modify:**
- `/home/damel/git/agent-tmux-manager/crates/atm/src/app.rs` — add `captured_output` field, capture polling logic
- `/home/damel/git/agent-tmux-manager/crates/atm/src/ui/detail_panel.rs` — render captured pane output
- `/home/damel/git/agent-tmux-manager/crates/atm/src/main.rs` — add capture tick event, wire up TmuxClient
- `/home/damel/git/agent-tmux-manager/crates/atm/src/input.rs` — add `CaptureUpdate` event variant
- `/home/damel/git/agent-tmux-manager/crates/atm/Cargo.toml` — add `atm-tmux` dependency

**What to do:**

**Step 6a: Add state to App**

In `app.rs`, add to `App` struct:
```rust
/// Captured terminal output from the selected agent's tmux pane.
/// Updated every ~1s by the capture polling task.
pub captured_output: Vec<String>,

/// The pane ID currently being captured (to detect selection changes).
pub capture_pane_id: Option<String>,
```

Initialize both in `App::new()`: `captured_output: Vec::new()`, `capture_pane_id: None`.

Add method:
```rust
pub fn update_capture(&mut self, pane_id: &str, lines: Vec<String>) {
    // Only update if this is still the selected pane
    if self.capture_pane_id.as_deref() == Some(pane_id) {
        self.captured_output = lines;
    }
}
```

**Step 6b: Add CaptureUpdate event**

In `input.rs`, add to the `Event` enum:
```rust
/// Updated pane capture output for a specific pane.
CaptureUpdate { pane_id: String, lines: Vec<String> },
```

**Step 6c: Spawn capture polling task**

In `main.rs`, after spawning the keyboard task, spawn a capture polling task:

```rust
fn spawn_capture_task(
    event_tx: mpsc::UnboundedSender<Event>,
    cancel_token: CancellationToken,
    capture_pane_rx: tokio::sync::watch::Receiver<Option<String>>,
) -> tokio::task::JoinHandle<()> {
    let client = atm_tmux::RealTmuxClient::new();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            if cancel_token.is_cancelled() { break; }
            
            let pane_id = capture_pane_rx.borrow().clone();
            if let Some(ref pane_id) = pane_id {
                match client.capture_pane(pane_id).await {
                    Ok(lines) => {
                        let _ = event_tx.send(Event::CaptureUpdate {
                            pane_id: pane_id.clone(),
                            lines,
                        });
                    }
                    Err(_) => {} // Pane gone, ignore
                }
            }
        }
    })
}
```

Use a `tokio::sync::watch` channel to communicate the currently-selected pane ID from the main loop to the capture task. When the user navigates to a different session, update the watch channel:

```rust
// In main(), before the event loop:
let (capture_pane_tx, capture_pane_rx) = tokio::sync::watch::channel(None::<String>);

// Spawn capture task
let capture_handle = spawn_capture_task(event_tx.clone(), cancel_token.clone(), capture_pane_rx);
```

In the event loop, after processing navigation events that change the selected session, update the watch:
```rust
// After any MoveDown/MoveUp/GoToRow/etc:
let new_pane = app.selected_session().and_then(|s| s.tmux_pane.clone());
if app.capture_pane_id != new_pane {
    app.capture_pane_id = new_pane.clone();
    app.captured_output.clear();
    let _ = capture_pane_tx.send(new_pane);
}
```

Handle the `CaptureUpdate` event:
```rust
Event::CaptureUpdate { pane_id, lines } => {
    app.update_capture(&pane_id, lines);
}
```

**Step 6d: Render captured output in detail panel**

In `detail_panel.rs`, modify `render_detail_panel_inline` to accept an additional `captured_output: &[String]` parameter:

```rust
pub fn render_detail_panel_inline(
    frame: &mut Frame,
    area: Rect,
    session: Option<&SessionView>,
    captured_output: &[String],
)
```

Split the detail area vertically: top portion (40%) shows session metadata (existing code), bottom portion (60%) shows captured terminal output.

For the capture section:
```rust
// Convert captured lines to ratatui Lines
let capture_lines: Vec<Line> = captured_output.iter()
    .map(|l| Line::from(l.as_str().to_string()))
    .collect();

let capture_block = Block::default()
    .title(" Terminal ")
    .borders(Borders::ALL)
    .border_style(Style::default().fg(Color::DarkGray));

// Show last N lines that fit in the area (scroll to bottom)
let visible_height = capture_area.height.saturating_sub(2) as usize;
let start = capture_lines.len().saturating_sub(visible_height);
let visible: Vec<Line> = capture_lines.into_iter().skip(start).collect();

let paragraph = Paragraph::new(visible).block(capture_block);
frame.render_widget(paragraph, capture_area);
```

Update all call sites in `ui/mod.rs`:
```rust
render_detail_panel_inline(frame, layout.detail_area, app.selected_session(), &app.captured_output);
```

**Step 6e: Add atm-tmux dependency to atm crate**

In `crates/atm/Cargo.toml`, add:
```toml
atm-tmux = { workspace = true }
```

**Verify:** `cargo test -p atm-tui` passes. `cargo build` succeeds. Manual test: run `atm` in tmux, navigate to a session, see terminal output in the detail panel.

---

## Task 7: `atm layout` CLI subcommand

**Files to modify:**
- `/home/damel/git/agent-tmux-manager/crates/atm/src/main.rs` — add `Layout` subcommand to `Command` enum

**What to do:**

Add to the `Command` enum:
```rust
/// Apply a layout template
Layout {
    /// Layout name: solo, pair, squad, grid, or a custom name
    name: String,
    /// Create a new tmux session instead of a window
    #[arg(long)]
    session: Option<String>,
    /// Split the current pane in-place (default: create new window)
    #[arg(long)]
    in_place: bool,
},
```

Handle in `main()`:
```rust
Some(Command::Layout { name, session, in_place }) => {
    let layout = atm_tmux::layout::load_layout(&name, None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    
    let target = if let Some(session_name) = session {
        atm_tmux::layout::LayoutTarget::NewSession(session_name)
    } else if in_place {
        // Get current pane ID from $TMUX_PANE
        let pane_id = std::env::var("TMUX_PANE")
            .map_err(|_| anyhow::anyhow!("TMUX_PANE not set — are you in tmux?"))?;
        atm_tmux::layout::LayoutTarget::CurrentPane(pane_id)
    } else {
        atm_tmux::layout::LayoutTarget::NewWindow(Some(name.clone()))
    };

    let client = atm_tmux::RealTmuxClient::new();
    let rt = tokio::runtime::Handle::current();
    let result = rt.block_on(atm_tmux::layout::apply_layout(&client, &layout, target))?;
    
    // Print pane assignments
    for (role, panes) in &result.panes {
        println!("{:?}: {}", role, panes.join(", "));
    }
    return Ok(());
}
```

**Verify:** `cargo build`. Run `atm layout solo --in-place` inside tmux and verify no errors. Run `atm layout pair` and verify a new window is created with splits.

---

## Task 8: Ship tmux-bindings.conf

**Files to modify:**
- `/home/damel/git/agent-tmux-manager/crates/atm/src/setup.rs` — add tmux bindings installation

**What to do:**

Add a function to `setup.rs`:
```rust
/// Installs the ATM tmux keybindings file to ~/.config/atm/tmux-bindings.conf.
fn install_tmux_bindings() -> Result<()> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?
        .join("atm");
    
    std::fs::create_dir_all(&config_dir)?;
    
    let bindings_path = config_dir.join("tmux-bindings.conf");
    let content = r#"# ATM — Agent Tmux Manager bindings
# Source this in your .tmux.conf: source-file ~/.config/atm/tmux-bindings.conf

# Spawn a new Claude agent in current project
bind C-n run-shell "atm spawn"

# Toggle ATM sidebar panel
bind C-a run-shell "atm toggle-panel"

# ATM popup overlay (alternative to sidebar)
bind C-s display-popup -E -w 35% -h 100% -x 0 "atm"

# Status bar integration (uncomment and add to status-right):
# set -g status-right '#(atm status) | %H:%M'
"#;
    
    std::fs::write(&bindings_path, content)?;
    println!("Installed tmux bindings: {}", bindings_path.display());
    println!("Add to your .tmux.conf: source-file {}", bindings_path.display());
    Ok(())
}
```

Call `install_tmux_bindings()` from the existing `setup()` function, after the hooks installation.

**Verify:** Run `atm setup`, verify `~/.config/atm/tmux-bindings.conf` exists with correct content.

---

## Build sequence

1. Task 1 (toml dep) — standalone
2. Task 2 (types + parsing) — depends on Task 1
3. Task 3 (presets) — depends on Task 2
4. Task 4 (engine) — depends on Task 3
5. Task 5 (config loading) — depends on Task 2, can parallel with Task 4
6. Task 6 (live capture) — independent of Tasks 2-5, can start in parallel
7. Task 7 (CLI subcommand) — depends on Tasks 4 + 5
8. Task 8 (tmux-bindings) — independent, can run anytime

Suggested batches:
- **Batch 1:** Tasks 1-3 (types foundation)
- **Batch 2:** Tasks 4-5 + 6 in parallel (engine + capture)
- **Batch 3:** Tasks 7-8 (CLI integration)
