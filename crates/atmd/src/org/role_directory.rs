//! The role directory a post's tags can reach (design 9.3, 9.4): role groups
//! for the policy, and the seats that have a role file, for the never-started
//! refusal. Read fresh per post on the shared detached role-reader slot.
//! Nothing is cached, so a role-file edit counts from the next post, and a
//! stored post keeps the seats it expanded to.
//!
//! The read fails soft. A busy or hung reader, or a roles path that cannot be
//! listed, leaves its projects out and says why in `problems`; inline tags
//! then stay unresolved, and a strict tag's refusal names the problem.

use super::agent_context::{detached_read, read_role_file};
use atm_org::{
    channel_policy::GroupDirectory, channel_query, mention::parse_mentions, AgentId, ChannelId,
    OrgState, PostTarget, ProjectId,
};
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Default)]
pub(super) struct RoleDirectory {
    pub groups: GroupDirectory,
    /// Every `<name>.md` read, valid or not.
    pub seats: BTreeSet<AgentId>,
    /// The projects whose roles could not be read, and why.
    pub problems: Vec<String>,
}

/// The project a short-form token names: a project channel's, or a reply's
/// root's when the root is in one. A direct channel has none: its short
/// names are its parties, and every party has a record.
pub(super) fn scope(state: &OrgState, target: &PostTarget) -> Option<ProjectId> {
    let channel = match target {
        PostTarget::Channel(channel) => Some(channel),
        PostTarget::Direct(_) => None,
        PostTarget::Reply(id) => channel_query::thread_root(state, *id)
            .and_then(|root| channel_query::post(state, root))
            .map(|post| &post.content.channel),
    };
    match channel? {
        ChannelId::Project(project) => Some(project.clone()),
        _ => None,
    }
}

/// The seat a strict tag names when it has a role file but no record
/// (design 9.4): the long form as written, the short form in the scope.
pub(super) fn never_started(
    state: &OrgState,
    target: &PostTarget,
    tag: &str,
    directory: &RoleDirectory,
) -> Option<AgentId> {
    let seat = match tag.split_once('/') {
        Some((project, name)) => AgentId::new(project.parse().ok()?, name).ok()?,
        None => AgentId::new(scope(state, target)?, tag).ok()?,
    };
    (state.agent(&seat).is_none() && directory.seats.contains(&seat)).then_some(seat)
}

/// The projects, with their recorded roots, whose role files the post's
/// tokens can name: the scope's for a short-form token other than `owner`,
/// and each long-form token's own. Empty when no token can name a role.
pub(super) fn directory_projects(
    state: &OrgState,
    target: &PostTarget,
    body: &str,
    tags: &[String],
) -> Vec<(ProjectId, PathBuf)> {
    let inline = parse_mentions(body)
        .into_iter()
        .map(|t| (t.project, t.name));
    let strict = tags.iter().map(|tag| match tag.split_once('/') {
        Some((project, name)) => (Some(project), name),
        None => (None, tag.as_str()),
    });
    let mut projects = BTreeSet::new();
    for (project, name) in inline.chain(strict) {
        projects.extend(match project {
            Some(project) => project.parse::<ProjectId>().ok(),
            None if name == "owner" => None,
            None => scope(state, target),
        });
    }
    projects
        .into_iter()
        .filter_map(|id| {
            let root = PathBuf::from(&state.project(&id)?.root);
            Some((id, root))
        })
        .collect()
}

/// Blocking: `<root>/.atm/roles/*.md` of each project, in name order. Each
/// file goes through agent_context's role reader; one that does not read or
/// parse still marks its seat as having a role file, with no group. A missing
/// roles directory is empty.
fn read_directory(projects: &[(ProjectId, PathBuf)]) -> RoleDirectory {
    let mut directory = RoleDirectory::default();
    for (project, root) in projects {
        let roles = root.join(".atm/roles");
        let listed = if root.is_absolute() {
            fs::read_dir(&roles).and_then(|entries| {
                entries
                    .map(|entry| entry.map(|e| e.path()))
                    .collect::<io::Result<Vec<_>>>()
            })
        } else {
            Err(io::Error::other(
                "the recorded project root is not absolute",
            ))
        };
        let mut paths = match listed {
            Ok(paths) => paths,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                let problem = format!("{project}: {}: {error}", roles.display());
                directory.problems.push(problem);
                continue;
            }
        };
        paths.sort();
        for path in paths {
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".md"))
            else {
                continue;
            };
            let Ok(seat) = AgentId::new(project.clone(), name) else {
                continue;
            };
            match read_role_file(&path, name) {
                Ok(None) => continue, // removed since the listing
                Ok(Some(role)) => {
                    for group in role.config.groups {
                        let key = (project.clone(), group);
                        directory.groups.entry(key).or_default().push(seat.clone());
                    }
                }
                Err(_) => {}
            }
            directory.seats.insert(seat);
        }
    }
    directory
}

/// The read on the shared role-reader slot, within agent_context's file
/// budget. A busy, hung or stopped reader gives an empty directory whose
/// `problems` says why: the post goes on without it.
pub(super) async fn role_directory(
    slot: Arc<Semaphore>,
    projects: Vec<(ProjectId, PathBuf)>,
) -> RoleDirectory {
    if projects.is_empty() {
        return RoleDirectory::default();
    }
    let directory = detached_read(slot, Path::new("the role directory"), move || {
        Ok(read_directory(&projects))
    })
    .await
    .unwrap_or_else(|error| RoleDirectory {
        problems: vec![error.to_string()],
        ..Default::default()
    });
    if !directory.problems.is_empty() {
        tracing::warn!(problems = ?directory.problems, "Role directory read in part");
    }
    directory
}
