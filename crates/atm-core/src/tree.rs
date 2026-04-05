//! Tree model for grouping sessions by project, worktree, and team.
//!
//! Transforms a flat list of [`SessionView`] into a hierarchical tree
//! structure for TUI rendering. The grouping hierarchy is:
//!
//! ```text
//! Project (git repo root)
//! ├── Worktree (branch / checkout path)
//! │   ├── Agent (session)
//! │   │   └── Subagent (child session)
//! │   └── Agent
//! └── Worktree
//!     └── ...
//! ```
//!
//! **Conditional worktree nesting:** When a project has only one worktree,
//! the worktree level is skipped — agents appear directly under the project.
//!
//! **Ungrouped sessions:** Sessions without a `project_root` are collected
//! under a synthetic "Other" project node.
//!
//! This module is pure logic with no TUI dependency, enabling reuse
//! in the future web UI.

use std::collections::{BTreeMap, HashSet};

use crate::{SessionId, SessionView};

// ============================================================================
// Tree Node Types
// ============================================================================

/// Unique identifier for a tree node, used for expand/collapse state tracking.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TreeNodeId {
    /// A project node, keyed by project root path.
    Project(String),
    /// A worktree node, keyed by worktree path.
    Worktree(String),
    /// A team node (placeholder for CC Agent Teams).
    Team(String),
    /// An agent (session) node, keyed by session ID.
    Agent(SessionId),
}

/// A node in the session grouping tree.
#[derive(Debug, Clone)]
pub enum TreeNode {
    /// A git project (repo root). Groups all agents across its worktrees.
    Project {
        /// Display name (last path component of project_root).
        name: String,
        /// Full project root path.
        root: String,
        /// Child nodes (Worktree or Agent when single-worktree).
        children: Vec<TreeNode>,
    },
    /// A git worktree within a project.
    Worktree {
        /// Full worktree path.
        path: String,
        /// Branch name (e.g., "main", "feature/auth").
        branch: Option<String>,
        /// Child nodes (Agent).
        children: Vec<TreeNode>,
    },
    /// Placeholder for CC Agent Teams integration (Phase 6).
    Team {
        /// Team name.
        name: String,
        /// Child nodes (Agent).
        children: Vec<TreeNode>,
    },
    /// A single session (leaf node).
    Agent {
        /// The session view data.
        session: SessionView,
        /// Child subagent sessions.
        subagents: Vec<TreeNode>,
    },
}

impl TreeNode {
    /// Returns the total number of agent sessions in this subtree.
    pub fn agent_count(&self) -> usize {
        match self {
            TreeNode::Project { children, .. }
            | TreeNode::Worktree { children, .. }
            | TreeNode::Team { children, .. } => children.iter().map(|c| c.agent_count()).sum(),
            TreeNode::Agent { subagents, .. } => {
                1 + subagents.iter().map(|s| s.agent_count()).sum::<usize>()
            }
        }
    }

    /// Returns true if any agent in this subtree needs attention.
    pub fn needs_attention(&self) -> bool {
        match self {
            TreeNode::Project { children, .. }
            | TreeNode::Worktree { children, .. }
            | TreeNode::Team { children, .. } => children.iter().any(|c| c.needs_attention()),
            TreeNode::Agent { session, subagents } => {
                session.needs_attention || subagents.iter().any(|s| s.needs_attention())
            }
        }
    }

    /// Returns the [`TreeNodeId`] for this node.
    pub fn node_id(&self) -> TreeNodeId {
        match self {
            TreeNode::Project { root, .. } => TreeNodeId::Project(root.clone()),
            TreeNode::Worktree { path, .. } => TreeNodeId::Worktree(path.clone()),
            TreeNode::Team { name, .. } => TreeNodeId::Team(name.clone()),
            TreeNode::Agent { session, .. } => TreeNodeId::Agent(session.id.clone()),
        }
    }
}

// ============================================================================
// Flattened Row (for cursor navigation)
// ============================================================================

/// The kind of row in the flattened tree.
#[derive(Debug, Clone)]
pub enum TreeRowKind {
    Project {
        name: String,
        root: String,
    },
    Worktree {
        path: String,
        branch: Option<String>,
    },
    Team {
        name: String,
    },
    Agent {
        session: SessionView,
    },
}

/// A single row in the flattened, navigable tree view.
///
/// Created by [`flatten_tree`] from a `Vec<TreeNode>` + expand/collapse state.
#[derive(Debug, Clone)]
pub struct TreeRow {
    /// Nesting depth (0 = top-level project, 1 = worktree/agent, etc.).
    pub depth: u8,
    /// Unique node identifier for expand/collapse state tracking.
    pub node_id: TreeNodeId,
    /// What kind of row this is.
    pub kind: TreeRowKind,
    /// Whether this node is currently expanded (only meaningful for non-leaf nodes).
    pub is_expanded: bool,
    /// Total agent count in this subtree (for collapsed groups).
    pub agent_count: usize,
    /// Whether any agent in the subtree needs attention (bubble-up).
    pub needs_attention: bool,
    /// Whether this node has children (is expandable).
    pub has_children: bool,
}

// ============================================================================
// Tree Building
// ============================================================================

/// Label for sessions that don't belong to any git project.
const UNGROUPED_PROJECT_NAME: &str = "Other";
const UNGROUPED_PROJECT_ROOT: &str = "__ungrouped__";

/// Builds a tree of [`TreeNode`] from a flat slice of sessions.
///
/// Grouping hierarchy: Project > Worktree (conditional) > Agent.
/// Sessions without `project_root` are grouped under an "Other" project.
/// Sessions with `parent_session_id` are nested under their parent.
pub fn build_tree(sessions: &[SessionView]) -> Vec<TreeNode> {
    if sessions.is_empty() {
        return Vec::new();
    }

    // Separate parent-level sessions from subagents
    let child_ids: HashSet<&SessionId> = sessions
        .iter()
        .filter(|s| s.parent_session_id.is_some())
        .map(|s| &s.id)
        .collect();

    // Index sessions by ID for subagent lookup
    let by_id: BTreeMap<&str, &SessionView> = sessions.iter().map(|s| (s.id.as_str(), s)).collect();

    // Group top-level sessions by project_root
    // BTreeMap for deterministic alphabetical ordering
    let mut by_project: BTreeMap<&str, Vec<&SessionView>> = BTreeMap::new();

    for session in sessions {
        // Skip subagents — they'll be nested under their parent
        if child_ids.contains(&session.id) {
            continue;
        }
        let project_key = session
            .project_root
            .as_deref()
            .unwrap_or(UNGROUPED_PROJECT_ROOT);
        by_project.entry(project_key).or_default().push(session);
    }

    let mut project_nodes = Vec::new();

    for (project_root, project_sessions) in &by_project {
        let project_name = if *project_root == UNGROUPED_PROJECT_ROOT {
            UNGROUPED_PROJECT_NAME.to_string()
        } else {
            extract_project_name(project_root)
        };

        // Build agent nodes (with subagent nesting)
        let make_agent_node = |session: &SessionView| -> TreeNode {
            let subagents: Vec<TreeNode> = session
                .child_session_ids
                .iter()
                .filter_map(|child_id| by_id.get(child_id.as_str()))
                .map(|child| TreeNode::Agent {
                    session: (*child).clone(),
                    subagents: Vec::new(), // No recursive subagent nesting for now
                })
                .collect();
            TreeNode::Agent {
                session: session.clone(),
                subagents,
            }
        };

        // Group by worktree within this project
        let mut by_worktree: BTreeMap<Option<&str>, Vec<&SessionView>> = BTreeMap::new();
        for session in project_sessions {
            let wt_key = session.worktree_path.as_deref();
            by_worktree.entry(wt_key).or_default().push(session);
        }

        // Conditional worktree nesting: skip worktree level if only one
        let skip_worktree = by_worktree.len() <= 1;

        if skip_worktree {
            // Agents directly under project
            let mut agents: Vec<TreeNode> = project_sessions
                .iter()
                .map(|s| make_agent_node(s))
                .collect();
            sort_agent_nodes(&mut agents);

            project_nodes.push(TreeNode::Project {
                name: project_name,
                root: project_root.to_string(),
                children: agents,
            });
        } else {
            // Worktree grouping
            let mut worktree_nodes: Vec<TreeNode> = Vec::new();
            for (wt_path, wt_sessions) in &by_worktree {
                let branch = wt_sessions.first().and_then(|s| s.worktree_branch.clone());
                let path = wt_path
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unknown".to_string());

                let mut agents: Vec<TreeNode> =
                    wt_sessions.iter().map(|s| make_agent_node(s)).collect();
                sort_agent_nodes(&mut agents);

                worktree_nodes.push(TreeNode::Worktree {
                    path,
                    branch,
                    children: agents,
                });
            }

            project_nodes.push(TreeNode::Project {
                name: project_name,
                root: project_root.to_string(),
                children: worktree_nodes,
            });
        }
    }

    project_nodes
}

/// Sort agent nodes by started_at descending (newest first).
fn sort_agent_nodes(nodes: &mut [TreeNode]) {
    nodes.sort_by(|a, b| {
        let a_time = match a {
            TreeNode::Agent { session, .. } => session.started_at.as_str(),
            _ => "",
        };
        let b_time = match b {
            TreeNode::Agent { session, .. } => session.started_at.as_str(),
            _ => "",
        };
        b_time.cmp(a_time) // newest first
    });
}

/// Extracts a short project name from a path (last component).
fn extract_project_name(path: &str) -> String {
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

// ============================================================================
// Tree Flattening
// ============================================================================

/// Flattens a tree into navigable rows, respecting expand/collapse state.
///
/// # Arguments
/// * `tree` — The tree nodes built by [`build_tree`].
/// * `expanded` — Set of [`TreeNodeId`]s that are currently expanded.
///   Nodes not in this set are collapsed (children hidden).
pub fn flatten_tree(tree: &[TreeNode], expanded: &HashSet<TreeNodeId>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for node in tree {
        flatten_node(node, 0, expanded, &mut rows);
    }
    rows
}

fn flatten_node(
    node: &TreeNode,
    depth: u8,
    expanded: &HashSet<TreeNodeId>,
    rows: &mut Vec<TreeRow>,
) {
    let node_id = node.node_id();
    let is_expanded = expanded.contains(&node_id);
    let has_children = match node {
        TreeNode::Project { children, .. }
        | TreeNode::Worktree { children, .. }
        | TreeNode::Team { children, .. } => !children.is_empty(),
        TreeNode::Agent { subagents, .. } => !subagents.is_empty(),
    };

    let kind = match node {
        TreeNode::Project { name, root, .. } => TreeRowKind::Project {
            name: name.clone(),
            root: root.clone(),
        },
        TreeNode::Worktree { path, branch, .. } => TreeRowKind::Worktree {
            path: path.clone(),
            branch: branch.clone(),
        },
        TreeNode::Team { name, .. } => TreeRowKind::Team { name: name.clone() },
        TreeNode::Agent { session, .. } => TreeRowKind::Agent {
            session: session.clone(),
        },
    };

    rows.push(TreeRow {
        depth,
        node_id: node_id.clone(),
        kind,
        is_expanded,
        agent_count: node.agent_count(),
        needs_attention: node.needs_attention(),
        has_children,
    });

    // Only recurse into children if expanded
    if is_expanded {
        let children: &[TreeNode] = match node {
            TreeNode::Project { children, .. }
            | TreeNode::Worktree { children, .. }
            | TreeNode::Team { children, .. } => children,
            TreeNode::Agent { subagents, .. } => subagents,
        };
        for child in children {
            flatten_node(child, depth.saturating_add(1), expanded, rows);
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Returns a set of all [`TreeNodeId`]s in the tree, useful for "expand all".
pub fn all_node_ids(tree: &[TreeNode]) -> HashSet<TreeNodeId> {
    let mut ids = HashSet::new();
    fn collect(node: &TreeNode, ids: &mut HashSet<TreeNodeId>) {
        ids.insert(node.node_id());
        match node {
            TreeNode::Project { children, .. }
            | TreeNode::Worktree { children, .. }
            | TreeNode::Team { children, .. } => {
                for child in children {
                    collect(child, ids);
                }
            }
            TreeNode::Agent { subagents, .. } => {
                for child in subagents {
                    collect(child, ids);
                }
            }
        }
    }
    for node in tree {
        collect(node, &mut ids);
    }
    ids
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionStatus;

    fn make_session(id: &str) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.get(..8).unwrap_or(id).to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            status: SessionStatus::Working,
            ..Default::default()
        }
    }

    fn make_session_in_project(
        id: &str,
        project_root: &str,
        worktree_path: &str,
        branch: &str,
        started_at: &str,
    ) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.get(..8).unwrap_or(id).to_string(),
            project_root: Some(project_root.to_string()),
            worktree_path: Some(worktree_path.to_string()),
            worktree_branch: Some(branch.to_string()),
            started_at: started_at.to_string(),
            status: SessionStatus::Working,
            ..Default::default()
        }
    }

    // ------------------------------------------------------------------
    // build_tree tests
    // ------------------------------------------------------------------

    #[test]
    fn test_empty_sessions() {
        let tree = build_tree(&[]);
        assert!(tree.is_empty());
    }

    #[test]
    fn test_ungrouped_sessions() {
        let sessions = vec![make_session("a"), make_session("b")];
        let tree = build_tree(&sessions);

        assert_eq!(tree.len(), 1);
        match &tree[0] {
            TreeNode::Project { name, children, .. } => {
                assert_eq!(name, "Other");
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected Project node"),
        }
    }

    #[test]
    fn test_single_worktree_skips_nesting() {
        let sessions = vec![
            make_session_in_project(
                "a",
                "/home/user/myapp",
                "/home/user/myapp",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session_in_project(
                "b",
                "/home/user/myapp",
                "/home/user/myapp",
                "main",
                "2026-01-01T00:01:00Z",
            ),
        ];
        let tree = build_tree(&sessions);

        assert_eq!(tree.len(), 1);
        match &tree[0] {
            TreeNode::Project { name, children, .. } => {
                assert_eq!(name, "myapp");
                // Should be Agent nodes directly (no Worktree nesting)
                assert_eq!(children.len(), 2);
                assert!(matches!(&children[0], TreeNode::Agent { .. }));
                assert!(matches!(&children[1], TreeNode::Agent { .. }));
            }
            _ => panic!("expected Project node"),
        }
    }

    #[test]
    fn test_multiple_worktrees_adds_nesting() {
        let sessions = vec![
            make_session_in_project(
                "a",
                "/home/user/myapp",
                "/home/user/myapp",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session_in_project(
                "b",
                "/home/user/myapp",
                "/home/user/myapp-auth",
                "feature/auth",
                "2026-01-01T00:01:00Z",
            ),
        ];
        let tree = build_tree(&sessions);

        assert_eq!(tree.len(), 1);
        match &tree[0] {
            TreeNode::Project { name, children, .. } => {
                assert_eq!(name, "myapp");
                // Should have 2 Worktree nodes
                assert_eq!(children.len(), 2);
                assert!(matches!(&children[0], TreeNode::Worktree { .. }));
                assert!(matches!(&children[1], TreeNode::Worktree { .. }));
            }
            _ => panic!("expected Project node"),
        }
    }

    #[test]
    fn test_multiple_projects() {
        let sessions = vec![
            make_session_in_project(
                "a",
                "/home/user/app-a",
                "/home/user/app-a",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session_in_project(
                "b",
                "/home/user/app-b",
                "/home/user/app-b",
                "main",
                "2026-01-01T00:00:00Z",
            ),
        ];
        let tree = build_tree(&sessions);

        // BTreeMap ordering: app-a before app-b
        assert_eq!(tree.len(), 2);
        match &tree[0] {
            TreeNode::Project { name, .. } => assert_eq!(name, "app-a"),
            _ => panic!("expected Project"),
        }
        match &tree[1] {
            TreeNode::Project { name, .. } => assert_eq!(name, "app-b"),
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn test_subagent_nesting() {
        let mut parent = make_session_in_project(
            "parent-1",
            "/home/user/myapp",
            "/home/user/myapp",
            "main",
            "2026-01-01T00:00:00Z",
        );
        parent.child_session_ids = vec![SessionId::new("child-1")];

        let mut child = make_session_in_project(
            "child-1",
            "/home/user/myapp",
            "/home/user/myapp",
            "main",
            "2026-01-01T00:00:01Z",
        );
        child.parent_session_id = Some(SessionId::new("parent-1"));

        let sessions = vec![parent, child];
        let tree = build_tree(&sessions);

        assert_eq!(tree.len(), 1);
        match &tree[0] {
            TreeNode::Project { children, .. } => {
                // Single worktree → agents directly under project
                assert_eq!(children.len(), 1, "child should be nested, not top-level");
                match &children[0] {
                    TreeNode::Agent {
                        session, subagents, ..
                    } => {
                        assert_eq!(session.id.as_str(), "parent-1");
                        assert_eq!(subagents.len(), 1);
                        match &subagents[0] {
                            TreeNode::Agent { session, .. } => {
                                assert_eq!(session.id.as_str(), "child-1");
                            }
                            _ => panic!("expected Agent subagent"),
                        }
                    }
                    _ => panic!("expected Agent"),
                }
            }
            _ => panic!("expected Project"),
        }
    }

    #[test]
    fn test_agent_count() {
        let sessions = vec![
            make_session_in_project(
                "a",
                "/home/user/myapp",
                "/home/user/myapp",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session_in_project(
                "b",
                "/home/user/myapp",
                "/home/user/myapp",
                "main",
                "2026-01-01T00:01:00Z",
            ),
            make_session_in_project(
                "c",
                "/home/user/myapp",
                "/home/user/myapp-wt",
                "dev",
                "2026-01-01T00:02:00Z",
            ),
        ];
        let tree = build_tree(&sessions);

        assert_eq!(tree[0].agent_count(), 3);
    }

    #[test]
    fn test_attention_bubbles_up() {
        let mut session = make_session_in_project(
            "alert-1",
            "/home/user/myapp",
            "/home/user/myapp",
            "main",
            "2026-01-01T00:00:00Z",
        );
        session.needs_attention = true;

        let normal = make_session_in_project(
            "normal-1",
            "/home/user/myapp",
            "/home/user/myapp",
            "main",
            "2026-01-01T00:01:00Z",
        );

        let tree = build_tree(&[session, normal]);
        assert!(
            tree[0].needs_attention(),
            "project should bubble up attention"
        );
    }

    #[test]
    fn test_agents_sorted_newest_first() {
        let sessions = vec![
            make_session_in_project(
                "old",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session_in_project(
                "new",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:05:00Z",
            ),
            make_session_in_project(
                "mid",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:02:00Z",
            ),
        ];
        let tree = build_tree(&sessions);

        match &tree[0] {
            TreeNode::Project { children, .. } => {
                let ids: Vec<&str> = children
                    .iter()
                    .filter_map(|c| match c {
                        TreeNode::Agent { session, .. } => Some(session.id.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(ids, vec!["new", "mid", "old"]);
            }
            _ => panic!("expected Project"),
        }
    }

    // ------------------------------------------------------------------
    // flatten_tree tests
    // ------------------------------------------------------------------

    #[test]
    fn test_flatten_empty() {
        let rows = flatten_tree(&[], &HashSet::new());
        assert!(rows.is_empty());
    }

    #[test]
    fn test_flatten_collapsed_project() {
        let sessions = vec![
            make_session_in_project(
                "a",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session_in_project(
                "b",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:01:00Z",
            ),
        ];
        let tree = build_tree(&sessions);
        let rows = flatten_tree(&tree, &HashSet::new()); // nothing expanded

        // Should only see the project row (collapsed)
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].is_expanded);
        assert_eq!(rows[0].agent_count, 2);
        assert!(rows[0].has_children);
    }

    #[test]
    fn test_flatten_expanded_project() {
        let sessions = vec![
            make_session_in_project(
                "a",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session_in_project(
                "b",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:01:00Z",
            ),
        ];
        let tree = build_tree(&sessions);

        let mut expanded = HashSet::new();
        expanded.insert(TreeNodeId::Project("/home/user/app".to_string()));

        let rows = flatten_tree(&tree, &expanded);

        // Project (expanded) + 2 agents = 3 rows
        assert_eq!(rows.len(), 3);
        assert!(rows[0].is_expanded);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].depth, 1);
    }

    #[test]
    fn test_flatten_with_worktrees() {
        let sessions = vec![
            make_session_in_project(
                "a",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session_in_project(
                "b",
                "/home/user/app",
                "/home/user/app-wt",
                "dev",
                "2026-01-01T00:01:00Z",
            ),
        ];
        let tree = build_tree(&sessions);

        // Expand everything
        let expanded = all_node_ids(&tree);
        let rows = flatten_tree(&tree, &expanded);

        // Project + Worktree(main) + Agent(a) + Worktree(dev) + Agent(b) = 5
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].depth, 0); // Project
        assert_eq!(rows[1].depth, 1); // Worktree
        assert_eq!(rows[2].depth, 2); // Agent
        assert_eq!(rows[3].depth, 1); // Worktree
        assert_eq!(rows[4].depth, 2); // Agent
    }

    #[test]
    fn test_flatten_subagent_nesting() {
        let mut parent = make_session_in_project(
            "parent",
            "/home/user/app",
            "/home/user/app",
            "main",
            "2026-01-01T00:00:00Z",
        );
        parent.child_session_ids = vec![SessionId::new("child")];

        let mut child = make_session_in_project(
            "child",
            "/home/user/app",
            "/home/user/app",
            "main",
            "2026-01-01T00:00:01Z",
        );
        child.parent_session_id = Some(SessionId::new("parent"));

        let tree = build_tree(&[parent, child]);
        let expanded = all_node_ids(&tree);
        let rows = flatten_tree(&tree, &expanded);

        // Project + Parent Agent + Child Agent = 3
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].depth, 0); // Project
        assert_eq!(rows[1].depth, 1); // Parent agent
        assert_eq!(rows[2].depth, 2); // Child subagent
    }

    // ------------------------------------------------------------------
    // Helper tests
    // ------------------------------------------------------------------

    #[test]
    fn test_extract_project_name() {
        assert_eq!(extract_project_name("/home/user/myapp"), "myapp");
        assert_eq!(extract_project_name("/home/user/my-project"), "my-project");
        assert_eq!(extract_project_name("single"), "single");
        assert_eq!(extract_project_name("/trailing/slash/"), "slash");
    }

    #[test]
    fn test_all_node_ids() {
        let sessions = vec![make_session_in_project(
            "a",
            "/home/user/app",
            "/home/user/app",
            "main",
            "2026-01-01T00:00:00Z",
        )];
        let tree = build_tree(&sessions);
        let ids = all_node_ids(&tree);

        assert!(ids.contains(&TreeNodeId::Project("/home/user/app".to_string())));
        assert!(ids.contains(&TreeNodeId::Agent(SessionId::new("a"))));
        assert_eq!(ids.len(), 2); // project + agent (no worktree since single)
    }

    #[test]
    fn test_mixed_grouped_and_ungrouped() {
        let sessions = vec![
            make_session_in_project(
                "a",
                "/home/user/app",
                "/home/user/app",
                "main",
                "2026-01-01T00:00:00Z",
            ),
            make_session("orphan"),
        ];
        let tree = build_tree(&sessions);

        // __ungrouped__ sorts before /home/... in BTreeMap
        assert_eq!(tree.len(), 2);
        let names: Vec<&str> = tree
            .iter()
            .filter_map(|n| match n {
                TreeNode::Project { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"Other"));
        assert!(names.contains(&"app"));
    }
}
