//! Coordinator tags stored in tmux's pane-level `@atm-role` option.

use std::collections::HashSet;

use atm_core::{SessionView, TreeNode};
use atm_tmux::{TmuxClient, TmuxError};

/// Parses `pane_id<TAB>role` rows, ignoring unset or unknown roles.
#[must_use]
pub fn parse_coordinator_panes(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, role)| *role == "coordinator")
        .map(|(pane, _)| pane.to_string())
        .collect()
}

/// A tagged pane makes a coordinator unless the session's parent shares it.
/// Pass the parent from the full, unfiltered session list; no or missing parent passes.
#[must_use]
pub fn is_coordinator(
    session: &SessionView,
    parent: Option<&SessionView>,
    panes: &HashSet<String>,
) -> bool {
    session.tmux_pane.as_ref().is_some_and(|pane| {
        panes.contains(pane)
            && !parent.is_some_and(|parent| parent.tmux_pane.as_ref() == Some(pane))
    })
}

/// Stably puts coordinators first within each group, preserving subagent order.
/// Group-level agents from `build_tree` have no parent in its input list;
/// children with present parents are in `Agent::subagents`, which we leave alone.
pub fn coordinators_first(tree: &mut [TreeNode], panes: &HashSet<String>) {
    for node in tree {
        match node {
            TreeNode::Project { children, .. }
            | TreeNode::Worktree { children, .. }
            | TreeNode::Team { children, .. } => {
                children.sort_by_key(|node| {
                    !matches!(node, TreeNode::Agent { session, .. }
                        if is_coordinator(session, None, panes))
                });
                coordinators_first(children, panes);
            }
            TreeNode::Agent { .. } => {}
        }
    }
}

/// Reads coordinator panes on the client's tmux server.
pub async fn coordinator_panes<C: TmuxClient + ?Sized>(
    client: &C,
) -> Result<HashSet<String>, TmuxError> {
    let output = client
        .raw_command("list-panes", &["-a", "-F", "#{pane_id}\t#{@atm-role}"])
        .await?;
    Ok(parse_coordinator_panes(&output))
}

/// Sets or removes the pane option, never a window/session option.
pub async fn set_coordinator<C: TmuxClient + ?Sized>(
    client: &C,
    pane: &str,
    on: bool,
) -> Result<(), TmuxError> {
    let args: &[&str] = if on {
        &["-p", "-t", pane, "@atm-role", "coordinator"]
    } else {
        &["-pu", "-t", pane, "@atm-role"]
    };
    client.raw_command("set-option", args).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_core::SessionId;
    use atm_tmux::{mock::MockCall, MockTmuxClient};

    fn agent(id: &str, time: &str, worktree: &str) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            started_at: time.into(),
            tmux_pane: Some(format!("%{id}")),
            project_root: Some("/project".into()),
            worktree_path: Some(worktree.into()),
            ..SessionView::default()
        }
    }

    fn row_ids(tree: &[atm_core::TreeNode]) -> Vec<atm_core::TreeNodeId> {
        atm_core::flatten_tree(tree, &atm_core::all_node_ids(tree))
            .into_iter()
            .map(|row| row.node_id)
            .collect()
    }

    fn agent_ids(tree: &[atm_core::TreeNode]) -> Vec<String> {
        row_ids(tree)
            .into_iter()
            .filter_map(|id| match id {
                atm_core::TreeNodeId::Agent(id) => Some(id.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn coordinator_bands_keep_newest_first_under_projects_worktrees_and_teams() {
        use atm_core::{build_tree, TreeNode};
        for worktrees in [false, true] {
            let mut sessions = vec![
                agent("old", "1", "/a"),
                agent("mid", "2", "/a"),
                agent("new", "3", "/a"),
                agent("newest", "4", "/a"),
            ];
            if worktrees {
                sessions.extend([agent("b-old", "1", "/b"), agent("b-new", "2", "/b")]);
            }
            let mut tree = build_tree(&sessions);
            let groups = |tree: &[TreeNode]| {
                row_ids(tree)
                    .into_iter()
                    .filter(|id| !matches!(id, atm_core::TreeNodeId::Agent(_)))
                    .collect::<Vec<_>>()
            };
            let before = groups(&tree);
            coordinators_first(&mut tree, &HashSet::from(["%old".into()]));
            let expected = if worktrees {
                vec!["old", "newest", "new", "mid", "b-new", "b-old"]
            } else {
                vec!["old", "newest", "new", "mid"]
            };
            assert_eq!(agent_ids(&tree), expected);
            assert_eq!(groups(&tree), before);

            // Start with build_tree's newest order, then preserve it in each band.
            let mut tree = build_tree(&sessions);
            coordinators_first(
                &mut tree,
                &HashSet::from(["%old".into(), "%new".into(), "%b-old".into()]),
            );
            let expected = if worktrees {
                vec!["new", "old", "newest", "mid", "b-old", "b-new"]
            } else {
                vec!["new", "old", "newest", "mid"]
            };
            assert_eq!(agent_ids(&tree), expected);
            assert_eq!(groups(&tree), before);
        }
        let mut teams = vec![TreeNode::Team {
            name: "team".into(),
            children: vec![
                TreeNode::Agent {
                    session: agent("new", "2", "/a"),
                    subagents: vec![],
                },
                TreeNode::Agent {
                    session: agent("old", "1", "/a"),
                    subagents: vec![],
                },
            ],
        }];
        coordinators_first(&mut teams, &HashSet::from(["%old".into()]));
        assert_eq!(agent_ids(&teams), ["old", "new"]);
    }

    #[test]
    fn coordinator_sort_leaves_descendants_in_spawn_order_but_promotes_orphans() {
        let lead = agent("lead", "1", "/a");
        let mut first = agent("first", "2", "/a");
        first.parent_session_id = Some(lead.id.clone());
        let mut mate = agent("mate", "3", "/a");
        mate.parent_session_id = Some(lead.id.clone());
        let mut child = agent("child", "4", "/a");
        child.parent_session_id = Some(mate.id.clone());
        child.tmux_pane.clone_from(&mate.tmux_pane);
        let newer = agent("newer", "5", "/a");
        let panes = HashSet::from(["%mate".into()]);
        let mut tree = atm_core::build_tree(&[
            lead,
            first.clone(),
            mate.clone(),
            child.clone(),
            newer.clone(),
        ]);
        coordinators_first(&mut tree, &panes);
        assert_eq!(
            agent_ids(&tree),
            ["newer", "lead", "first", "mate", "child"]
        );
        let mut tree = atm_core::build_tree(&[first, mate, child, newer]);
        coordinators_first(&mut tree, &panes);
        assert_eq!(agent_ids(&tree), ["mate", "child", "newer", "first"]);
    }

    #[test]
    fn empty_coordinator_set_preserves_the_entire_tree() {
        let mut child = agent("child", "2", "/a");
        child.parent_session_id = Some(SessionId::new("lead"));
        let mut tree =
            atm_core::build_tree(&[agent("lead", "1", "/a"), child, agent("other", "3", "/b")]);
        let before = format!("{tree:?}");
        coordinators_first(&mut tree, &HashSet::new());
        assert_eq!(format!("{tree:?}"), before);
    }

    #[test]
    fn parses_only_exact_coordinator_roles() {
        assert_eq!(
            parse_coordinator_panes("%0\tcoordinator\n%1\t\n%2\timplementer\nno tab\n%3\tCoordinator\n%4\tcoordinator \n%5\tcoordinator\textra\n%0\tcoordinator\n"),
            HashSet::from(["%0".to_string()])
        );
        assert!(parse_coordinator_panes("").is_empty());
    }

    #[test]
    fn tagged_panes_exclude_only_children_sharing_their_parents_pane() {
        let panes = HashSet::from(["%7".to_string(), "%8".to_string()]);
        let lead = SessionView {
            id: SessionId::new("lead"),
            tmux_pane: Some("%7".into()),
            ..SessionView::default()
        };
        let teammate = SessionView {
            id: SessionId::new("teammate"),
            parent_session_id: Some(lead.id.clone()),
            tmux_pane: Some("%8".into()),
            ..SessionView::default()
        };
        let parent_without_pane = SessionView {
            id: SessionId::new("no-pane"),
            ..SessionView::default()
        };
        for (name, pane, parent_id, parent, expected) in [
            ("tagged lead", Some("%7"), None, None, true),
            (
                "lead's subagent",
                Some("%7"),
                Some("lead"),
                Some(&lead),
                false,
            ),
            ("teammate", Some("%8"), Some("lead"), Some(&lead), true),
            ("missing parent", Some("%7"), Some("missing"), None, true),
            (
                "teammate's subagent",
                Some("%8"),
                Some("teammate"),
                Some(&teammate),
                false,
            ),
            ("no pane", None, None, None, false),
            ("untagged pane", Some("%9"), None, None, false),
            (
                "parent without pane",
                Some("%7"),
                Some("no-pane"),
                Some(&parent_without_pane),
                true,
            ),
        ] {
            let session = SessionView {
                tmux_pane: pane.map(str::to_string),
                parent_session_id: parent_id.map(SessionId::new),
                ..SessionView::default()
            };
            assert_eq!(is_coordinator(&session, parent, &panes), expected, "{name}");
        }
    }

    #[tokio::test]
    async fn raw_commands_read_roles_and_only_write_pane_options() {
        let client = MockTmuxClient::new();
        client.set_next_raw_output("%7\tcoordinator\n%8\t\n");
        assert_eq!(
            coordinator_panes(&client).await.unwrap(),
            HashSet::from(["%7".to_string()])
        );
        set_coordinator(&client, "%7", true).await.unwrap();
        set_coordinator(&client, "%7", false).await.unwrap();
        assert_eq!(
            client.calls(),
            vec![
                MockCall::RawCommand {
                    subcommand: "list-panes".into(),
                    args: vec!["-a".into(), "-F".into(), "#{pane_id}\t#{@atm-role}".into()]
                },
                MockCall::RawCommand {
                    subcommand: "set-option".into(),
                    args: ["-p", "-t", "%7", "@atm-role", "coordinator"]
                        .map(str::to_string)
                        .to_vec()
                },
                MockCall::RawCommand {
                    subcommand: "set-option".into(),
                    args: ["-pu", "-t", "%7", "@atm-role"]
                        .map(str::to_string)
                        .to_vec()
                },
            ]
        );
    }

    #[tokio::test]
    async fn tmux_errors_are_returned() {
        let client = MockTmuxClient::new();
        client.set_next_error(atm_tmux::TmuxError::NotFound);
        assert!(matches!(
            coordinator_panes(&client).await,
            Err(atm_tmux::TmuxError::NotFound)
        ));
        for on in [true, false] {
            client.set_next_error(atm_tmux::TmuxError::Timeout {
                command: "set-option".into(),
                timeout: std::time::Duration::from_secs(5),
            });
            assert!(matches!(
                set_coordinator(&client, "%7", on).await,
                Err(atm_tmux::TmuxError::Timeout { .. })
            ));
        }
    }
}
