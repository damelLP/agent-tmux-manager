//! Layout template system for ATM.
//!
//! Defines a tree of [`Slot`]s that ATM materializes into tmux panes.
//! Includes TOML parsing, built-in presets, layout application, and config
//! file loading.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde::Deserialize;

use crate::{TmuxClient, TmuxError};

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

fn default_count() -> u8 {
    1
}

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

/// Parses a [`Layout`] from a TOML string.
///
/// The TOML should have a top-level `[layout]` table containing `name` and `root`.
pub fn parse_layout(toml_str: &str) -> Result<Layout, LayoutConfigError> {
    #[derive(Deserialize)]
    struct Wrapper {
        layout: Layout,
    }
    let wrapper: Wrapper =
        toml::from_str(toml_str).map_err(|e| LayoutConfigError::Parse(e.to_string()))?;
    Ok(wrapper.layout)
}

// ---------------------------------------------------------------------------
// Built-in preset layouts
// ---------------------------------------------------------------------------

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
            role: SlotRole::Shell,
            size: "100%".to_string(),
            direction: SplitDirection::Horizontal,
            children: vec![
                Slot {
                    role: SlotRole::Shell,
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
/// 75% left with 3 agent panes (2 top side-by-side, 1 bottom full-width), 25% right ATM sidebar.
pub fn preset_squad() -> Layout {
    Layout {
        name: "squad".to_string(),
        root: Slot {
            role: SlotRole::Shell,
            size: "100%".to_string(),
            direction: SplitDirection::Horizontal,
            children: vec![
                Slot {
                    role: SlotRole::Shell,
                    size: "75%".to_string(),
                    direction: SplitDirection::Vertical,
                    children: vec![
                        // Top row: 2 agents side-by-side (66% of height)
                        Slot {
                            role: SlotRole::Shell,
                            size: "66%".to_string(),
                            direction: SplitDirection::Horizontal,
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
                        // Bottom row: 1 agent full-width (33% of height)
                        Slot {
                            role: SlotRole::Agent,
                            size: "33%".to_string(),
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

/// Returns the built-in "grid" layout:
/// 2x2 grid of 4 agent panes. ATM via popup.
pub fn preset_grid() -> Layout {
    Layout {
        name: "grid".to_string(),
        root: Slot {
            role: SlotRole::Shell,
            size: "100%".to_string(),
            direction: SplitDirection::Vertical,
            children: vec![
                // Top row: 2 agents side-by-side
                Slot {
                    role: SlotRole::Shell,
                    size: "50%".to_string(),
                    direction: SplitDirection::Horizontal,
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
                // Bottom row: 2 agents side-by-side
                Slot {
                    role: SlotRole::Shell,
                    size: "50%".to_string(),
                    direction: SplitDirection::Horizontal,
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
            ],
            count: 1,
        },
    }
}

/// Returns the built-in "workspace" layout:
/// Fixed-width ATM sidebar (30 cols) on the left, workspace on the right.
///
/// Workspace is the first child (inherits full pane, gets split into
/// agent 80% + shell 20%). ATM panel then splits off as a 30-column
/// pane on the left using tmux's `-b` (before) flag — but since our
/// split_window doesn't support `-b`, we handle this in cmd_workspace
/// by splitting off ATM at 30 columns and then swapping panes.
///
/// Instead, we use the approach: workspace first, ATM splits off.
/// The ATM split uses "30" (absolute columns, not percentage).
/// tmux `split-window -h -l 30` from the workspace pane creates a
/// 30-column pane to the right. We then swap it to the left in cmd_workspace.
pub fn preset_workspace() -> Layout {
    Layout {
        name: "workspace".to_string(),
        root: Slot {
            role: SlotRole::Shell,
            size: "100%".to_string(),
            direction: SplitDirection::Horizontal,
            children: vec![
                // Workspace (first child, inherits full pane)
                Slot {
                    role: SlotRole::Shell,
                    size: "100%".to_string(),
                    direction: SplitDirection::Vertical,
                    children: vec![
                        Slot {
                            role: SlotRole::Agent,
                            size: "80%".to_string(),
                            direction: SplitDirection::Horizontal,
                            children: vec![],
                            count: 1,
                        },
                        Slot {
                            role: SlotRole::Shell,
                            size: "20%".to_string(),
                            direction: SplitDirection::Horizontal,
                            children: vec![],
                            count: 1,
                        },
                    ],
                    count: 1,
                },
                // ATM sidebar splits off at 30 columns
                Slot {
                    role: SlotRole::AtmPanel,
                    size: "30".to_string(),
                    direction: SplitDirection::Horizontal,
                    children: vec![],
                    count: 1,
                },
            ],
            count: 1,
        },
    }
}

/// Returns the built-in "workspace-editor" layout:
/// Fixed-width ATM sidebar (30 cols) on the left, workspace with editor+agent over shell.
pub fn preset_workspace_editor() -> Layout {
    Layout {
        name: "workspace-editor".to_string(),
        root: Slot {
            role: SlotRole::Shell,
            size: "100%".to_string(),
            direction: SplitDirection::Horizontal,
            children: vec![
                // Workspace (first child, inherits full pane)
                Slot {
                    role: SlotRole::Shell,
                    size: "100%".to_string(),
                    direction: SplitDirection::Vertical,
                    children: vec![
                        Slot {
                            role: SlotRole::Shell,
                            size: "80%".to_string(),
                            direction: SplitDirection::Horizontal,
                            children: vec![
                                Slot {
                                    role: SlotRole::Editor,
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
                            role: SlotRole::Shell,
                            size: "20%".to_string(),
                            direction: SplitDirection::Horizontal,
                            children: vec![],
                            count: 1,
                        },
                    ],
                    count: 1,
                },
                // ATM sidebar splits off at 30 columns
                Slot {
                    role: SlotRole::AtmPanel,
                    size: "30".to_string(),
                    direction: SplitDirection::Horizontal,
                    children: vec![],
                    count: 1,
                },
            ],
            count: 1,
        },
    }
}

/// Returns a preset layout by name, or `None` if not found.
pub fn preset_by_name(name: &str) -> Option<Layout> {
    match name {
        "solo" => Some(preset_solo()),
        "pair" => Some(preset_pair()),
        "squad" => Some(preset_squad()),
        "grid" => Some(preset_grid()),
        "workspace" => Some(preset_workspace()),
        "workspace-editor" => Some(preset_workspace_editor()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Layout engine
// ---------------------------------------------------------------------------

/// Applies a layout template by creating tmux panes according to the slot tree.
///
/// Returns a [`LayoutResult`] mapping slot roles to their created pane IDs.
pub async fn apply_layout(
    client: &(dyn TmuxClient + Send + Sync),
    layout: &Layout,
    target: LayoutTarget,
) -> Result<LayoutResult, TmuxError> {
    let root_pane = match &target {
        LayoutTarget::CurrentPane(pane_id) => pane_id.clone(),
        LayoutTarget::NewWindow(_name) => {
            // Derive current session from $TMUX_PANE (set by tmux) via list_panes
            let current_pane = std::env::var("TMUX_PANE").unwrap_or_default();
            let session = if !current_pane.is_empty() {
                let panes = client.list_panes().await.unwrap_or_default();
                panes
                    .iter()
                    .find(|p| p.pane_id == current_pane)
                    .map(|p| p.session_name.clone())
                    .unwrap_or_else(|| "0".to_string())
            } else {
                "0".to_string()
            };
            client.new_window(&session, None).await?
        }
        LayoutTarget::NewSession(name) => client.new_session(name).await?,
    };

    let mut result = LayoutResult {
        panes: HashMap::new(),
    };
    apply_slot(client, &layout.root, &root_pane, &mut result).await?;
    Ok(result)
}

/// Recursively applies a slot, splitting the parent pane for each child.
///
/// Uses `Pin<Box<dyn Future>>` because async functions cannot be directly
/// recursive.
fn apply_slot<'a>(
    client: &'a (dyn TmuxClient + Send + Sync),
    slot: &'a Slot,
    pane_id: &'a str,
    result: &'a mut LayoutResult,
) -> Pin<Box<dyn Future<Output = Result<(), TmuxError>> + Send + 'a>> {
    Box::pin(async move {
        if slot.children.is_empty() {
            // Leaf node — record pane under its role
            result
                .panes
                .entry(slot.role)
                .or_default()
                .push(pane_id.to_string());
            return Ok(());
        }

        // Container: first child inherits current pane, subsequent children split off.
        let first = slot
            .children
            .first()
            .ok_or_else(|| TmuxError::ParseError("container slot has no children".to_string()))?;

        // Recurse into first child (it inherits the current pane)
        apply_slot(client, first, pane_id, result).await?;

        // Split off remaining children
        for child in slot.children.iter().skip(1) {
            // SplitDirection::Vertical means children stack top-to-bottom → horizontal=true (uses -v)
            // SplitDirection::Horizontal means children sit left-to-right → horizontal=false (uses -h)
            let horizontal = matches!(slot.direction, SplitDirection::Vertical);
            let new_pane = client
                .split_window(pane_id, &child.size, horizontal, None)
                .await?;
            apply_slot(client, child, &new_pane, result).await?;
        }

        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Config file loading
// ---------------------------------------------------------------------------

/// Loads a layout by name, checking (in order):
/// 1. Project-local `.atm/layout.toml`
/// 2. Global `~/.config/atm/config.toml`
/// 3. Built-in presets
pub fn load_layout(name: &str, project_root: Option<&Path>) -> Result<Layout, LayoutConfigError> {
    // 1. Project-local config
    if let Some(root) = project_root {
        let config_path = root.join(".atm").join("layout.toml");
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            let layout = parse_layout(&content)?;
            if layout.name == name {
                return Ok(layout);
            }
        }
    }

    // 2. Global config
    if let Some(config_dir) = dirs::config_dir() {
        let global_path = config_dir.join("atm").join("config.toml");
        if global_path.exists() {
            let content = std::fs::read_to_string(&global_path)?;
            if let Ok(layout) = parse_layout(&content) {
                if layout.name == name {
                    return Ok(layout);
                }
            }
        }
    }

    // 3. Built-in preset
    preset_by_name(name).ok_or_else(|| LayoutConfigError::NotFound(name.to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// Counts the total number of agent slots in a layout tree.
    fn count_agent_slots(slot: &super::Slot) -> usize {
        let self_count = if slot.role == super::SlotRole::Agent {
            1
        } else {
            0
        };
        let child_count: usize = slot.children.iter().map(count_agent_slots).sum();
        self_count + child_count
    }
    use super::*;

    // -- Task 2: Parsing tests -----------------------------------------------

    #[test]
    fn parse_valid_layout() {
        let toml_str = r#"
[layout]
name = "my-layout"

[layout.root]
role = "shell"
size = "100%"
direction = "horizontal"

[[layout.root.children]]
role = "agent"
size = "75%"
direction = "vertical"

[[layout.root.children]]
role = "atm_panel"
size = "25%"
direction = "vertical"
"#;
        let layout = parse_layout(toml_str).unwrap();
        assert_eq!(layout.name, "my-layout");
        assert_eq!(layout.root.role, SlotRole::Shell);
        assert_eq!(layout.root.children.len(), 2);
        assert_eq!(layout.root.children[0].role, SlotRole::Agent);
        assert_eq!(layout.root.children[1].role, SlotRole::AtmPanel);
    }

    #[test]
    fn parse_minimal_layout() {
        let toml_str = r#"
[layout]
name = "minimal"

[layout.root]
role = "agent"
size = "100%"
direction = "horizontal"
"#;
        let layout = parse_layout(toml_str).unwrap();
        assert_eq!(layout.name, "minimal");
        assert_eq!(layout.root.role, SlotRole::Agent);
        assert!(layout.root.children.is_empty());
        assert_eq!(layout.root.count, 1); // default
    }

    #[test]
    fn parse_defaults_applied() {
        let toml_str = r#"
[layout]
name = "defaults-test"

[layout.root]
role = "agent"
size = "100%"
direction = "horizontal"
"#;
        let layout = parse_layout(toml_str).unwrap();
        // count defaults to 1
        assert_eq!(layout.root.count, 1);
        // children defaults to empty
        assert!(layout.root.children.is_empty());
    }

    #[test]
    fn parse_explicit_count() {
        let toml_str = r#"
[layout]
name = "counted"

[layout.root]
role = "agent"
size = "100%"
direction = "horizontal"
count = 3
"#;
        let layout = parse_layout(toml_str).unwrap();
        assert_eq!(layout.root.count, 3);
    }

    #[test]
    fn parse_invalid_toml_returns_error() {
        let result = parse_layout("this is not valid toml [[[");
        assert!(result.is_err());
        match result {
            Err(LayoutConfigError::Parse(msg)) => {
                assert!(!msg.is_empty());
            }
            _ => panic!("expected Parse error"),
        }
    }

    #[test]
    fn parse_missing_layout_table_returns_error() {
        let toml_str = r#"
[something_else]
name = "nope"
"#;
        assert!(parse_layout(toml_str).is_err());
    }

    // -- Task 3: Preset tests ------------------------------------------------

    #[test]
    fn preset_solo_structure() {
        let layout = preset_solo();
        assert_eq!(layout.name, "solo");
        assert_eq!(layout.root.role, SlotRole::Agent);
        assert!(layout.root.children.is_empty());
        assert_eq!(count_agent_slots(&layout.root), 1);
    }

    #[test]
    fn preset_pair_structure() {
        let layout = preset_pair();
        assert_eq!(layout.name, "pair");
        assert_eq!(layout.root.role, SlotRole::Shell);
        assert_eq!(layout.root.children.len(), 2);
        assert_eq!(count_agent_slots(&layout.root), 2);
        // Right child is ATM panel
        assert_eq!(layout.root.children[1].role, SlotRole::AtmPanel);
    }

    #[test]
    fn preset_squad_structure() {
        let layout = preset_squad();
        assert_eq!(layout.name, "squad");
        assert_eq!(count_agent_slots(&layout.root), 3);
        // Right child is ATM panel
        assert_eq!(layout.root.children[1].role, SlotRole::AtmPanel);
    }

    #[test]
    fn preset_grid_structure() {
        let layout = preset_grid();
        assert_eq!(layout.name, "grid");
        assert_eq!(count_agent_slots(&layout.root), 4);
        // Grid has no ATM panel (uses popup)
        assert_eq!(layout.root.children.len(), 2);
        assert_eq!(layout.root.children[0].role, SlotRole::Shell);
        assert_eq!(layout.root.children[1].role, SlotRole::Shell);
    }

    #[test]
    fn preset_workspace_structure() {
        let layout = preset_workspace();
        assert_eq!(layout.name, "workspace");
        assert_eq!(count_agent_slots(&layout.root), 1);
        // Root has workspace container + ATM panel
        assert_eq!(layout.root.children.len(), 2);
        // Workspace container (first) has agent + shell
        let ws = &layout.root.children[0];
        assert_eq!(ws.children.len(), 2);
        assert_eq!(ws.children[0].role, SlotRole::Agent);
        assert_eq!(ws.children[1].role, SlotRole::Shell);
        // ATM panel is second (split off to right, then swapped to left)
        assert_eq!(layout.root.children[1].role, SlotRole::AtmPanel);
    }

    #[test]
    fn preset_workspace_editor_structure() {
        let layout = preset_workspace_editor();
        assert_eq!(layout.name, "workspace-editor");
        assert_eq!(count_agent_slots(&layout.root), 1);
        // Root has workspace container + ATM panel
        assert_eq!(layout.root.children.len(), 2);
        // Workspace container (first) has main container + shell
        let ws = &layout.root.children[0];
        assert_eq!(ws.children.len(), 2);
        // Main container has editor + agent
        let main = &ws.children[0];
        assert_eq!(main.children.len(), 2);
        assert_eq!(main.children[0].role, SlotRole::Editor);
        assert_eq!(main.children[1].role, SlotRole::Agent);
        assert_eq!(ws.children[1].role, SlotRole::Shell);
        // ATM panel is second
        assert_eq!(layout.root.children[1].role, SlotRole::AtmPanel);
    }

    #[test]
    fn preset_by_name_known() {
        assert!(preset_by_name("solo").is_some());
        assert!(preset_by_name("pair").is_some());
        assert!(preset_by_name("squad").is_some());
        assert!(preset_by_name("grid").is_some());
        assert!(preset_by_name("workspace").is_some());
        assert!(preset_by_name("workspace-editor").is_some());
    }

    #[test]
    fn preset_by_name_unknown() {
        assert!(preset_by_name("nonexistent").is_none());
        assert!(preset_by_name("").is_none());
    }

    #[test]
    fn preset_by_name_returns_correct_layout() {
        let solo = preset_by_name("solo").unwrap();
        assert_eq!(solo.name, "solo");

        let pair = preset_by_name("pair").unwrap();
        assert_eq!(pair.name, "pair");
    }

    // -- Task 4: apply_layout tests -------------------------------------------

    use crate::mock::{MockCall, MockTmuxClient};

    #[tokio::test]
    async fn apply_layout_solo_current_pane_no_splits() {
        let mock = MockTmuxClient::new();
        let layout = preset_solo();
        let target = LayoutTarget::CurrentPane("%1".to_string());

        let result = apply_layout(&mock, &layout, target).await.unwrap();

        // Solo has no children — no splits should occur
        let calls = mock.calls();
        assert!(
            calls.is_empty(),
            "solo layout should not produce any splits"
        );

        // One Agent pane recorded with the original pane ID
        let agent_panes = result.panes.get(&SlotRole::Agent).unwrap();
        assert_eq!(agent_panes, &["%1".to_string()]);
    }

    #[tokio::test]
    async fn apply_layout_pair_current_pane_splits_and_roles() {
        let mock = MockTmuxClient::new();
        // Pair layout splits:
        //   root (Shell, Horizontal) -> 2 children
        //     child[0] (Shell, Vertical) inherits %1 -> 2 children
        //       child[0][0] (Agent) inherits %1 — no split
        //       child[0][1] (Agent) splits off from %1 → needs pane id
        //     child[1] (AtmPanel) splits off from %1 → needs pane id
        //
        // The horizontal split for the ATM panel happens from root direction=Horizontal → horizontal=false
        // The vertical split for 2nd agent happens from child[0] direction=Vertical → horizontal=true
        //
        // Order: first recurse into child[0], which recurses into child[0][0] (leaf, no split),
        // then splits child[0][1] (vertical split from %1), then splits child[1] (horizontal split from %1).
        mock.set_next_pane_id("%2"); // child[0][1] agent split
        mock.set_next_pane_id("%3"); // child[1] ATM panel split

        let layout = preset_pair();
        let target = LayoutTarget::CurrentPane("%1".to_string());

        let result = apply_layout(&mock, &layout, target).await.unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 2, "pair layout should produce 2 splits");

        // First split: 2nd agent from the agent container (direction=Vertical → horizontal=true)
        assert!(matches!(
            &calls[0],
            MockCall::SplitWindow {
                target,
                size,
                horizontal: true,
                command: None,
            } if target == "%1" && size == "50%"
        ));

        // Second split: ATM panel from root (direction=Horizontal → horizontal=false)
        assert!(matches!(
            &calls[1],
            MockCall::SplitWindow {
                target,
                size,
                horizontal: false,
                command: None,
            } if target == "%1" && size == "25%"
        ));

        // Check role mappings
        let agent_panes = result.panes.get(&SlotRole::Agent).unwrap();
        assert_eq!(agent_panes.len(), 2);
        assert_eq!(agent_panes[0], "%1");
        assert_eq!(agent_panes[1], "%2");

        let atm_panes = result.panes.get(&SlotRole::AtmPanel).unwrap();
        assert_eq!(atm_panes, &["%3".to_string()]);
    }

    #[tokio::test]
    async fn apply_layout_new_window_calls_new_window_first() {
        let mock = MockTmuxClient::new();
        mock.set_next_pane_id("%10"); // returned by new_window

        let layout = preset_solo();
        let target = LayoutTarget::NewWindow(Some("test-win".to_string()));

        let result = apply_layout(&mock, &layout, target).await.unwrap();

        let calls = mock.calls();
        // If TMUX_PANE is set (running inside tmux), list_panes is called first
        // to resolve the current session name. Then new_window is called.
        let new_window_call = calls
            .iter()
            .find(|c| matches!(c, MockCall::NewWindow { .. }));
        assert!(
            new_window_call.is_some(),
            "expected NewWindow call in {calls:?}"
        );

        // The solo agent should be recorded with the pane ID from new_window
        let agent_panes = result.panes.get(&SlotRole::Agent).unwrap();
        assert_eq!(agent_panes, &["%10".to_string()]);
    }

    #[tokio::test]
    async fn apply_layout_new_session_calls_new_session_first() {
        let mock = MockTmuxClient::new();
        mock.set_next_pane_id("%20"); // returned by new_session

        let layout = preset_solo();
        let target = LayoutTarget::NewSession("my-session".to_string());

        let result = apply_layout(&mock, &layout, target).await.unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            &calls[0],
            MockCall::NewSession { name } if name == "my-session"
        ));

        let agent_panes = result.panes.get(&SlotRole::Agent).unwrap();
        assert_eq!(agent_panes, &["%20".to_string()]);
    }

    // -- Task 5: load_layout tests --------------------------------------------

    #[test]
    fn load_layout_returns_builtin_preset() {
        let layout = load_layout("solo", None).unwrap();
        assert_eq!(layout.name, "solo");
    }

    #[test]
    fn load_layout_not_found_returns_error() {
        let result = load_layout("nonexistent", None);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            LayoutConfigError::NotFound(name) if name == "nonexistent"
        ));
    }

    #[test]
    fn load_layout_project_config_overrides_preset() {
        let tmp = std::env::temp_dir().join("atm-test-load-layout");
        let atm_dir = tmp.join(".atm");
        let _ = std::fs::create_dir_all(&atm_dir);

        let toml_content = r#"
[layout]
name = "solo"

[layout.root]
role = "editor"
size = "100%"
direction = "horizontal"
count = 1
"#;
        let config_path = atm_dir.join("layout.toml");
        std::fs::write(&config_path, toml_content).unwrap();

        let layout = load_layout("solo", Some(&tmp)).unwrap();
        // The project config has role=editor instead of the built-in solo's role=agent
        assert_eq!(layout.name, "solo");
        assert_eq!(layout.root.role, SlotRole::Editor);

        // Clean up
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
