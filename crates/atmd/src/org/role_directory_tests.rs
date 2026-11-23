//! The role directory (unit U10): which projects a post's tokens make the
//! daemon read, and what a read of `.atm/roles` yields. Real files in temp
//! roots on the real clock (the read is a detached thread, which a paused
//! clock would time out); the busy-slot case holds the slot on a paused clock
//! and starts no thread. Role files are hand-written fixtures.

use super::message_service_tests::{guarded, owner, Rig};
use super::role_directory::{directory_projects, role_directory, RoleDirectory};
use atm_comms::channel_types::ChannelPostParams;
use atm_org::{
    Address, AgentId, ChannelId, MessageId, OrgCommand, OrgState, PostTarget, ProjectId,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Semaphore;

fn project(id: &str) -> ProjectId {
    id.parse().unwrap()
}

fn seat(id: &str) -> AgentId {
    id.parse().unwrap()
}

/// `<root>/.atm/roles/<name>.md` with these groups.
pub(super) fn role(root: &Path, name: &str, groups: &[&str]) {
    let dir = root.join(".atm/roles");
    fs::create_dir_all(&dir).unwrap();
    let text = format!("+++\nharness = 'pi'\ngroups = {groups:?}\n+++\nA fixture role.\n");
    fs::write(dir.join(format!("{name}.md")), text).unwrap();
}

/// The seeded rig plus `demo/lead` (so the `demo` channel is listed), with
/// `demo` moved to `demo_root` and `acme` registered at `acme_root`.
async fn rooted(demo_root: &Path, acme_root: &Path) -> Rig {
    let rig = Rig::seeded().await;
    rig.run(OrgCommand::Adopt {
        agent_id: seat("demo/lead"),
        pane: "%3".into(),
        session_id: Some("s3".into()),
    })
    .await;
    for (id, root, move_root) in [("demo", demo_root, true), ("acme", acme_root, false)] {
        rig.run(OrgCommand::RegisterProject {
            project_id: project(id),
            root: root.to_str().unwrap().into(),
            move_root,
        })
        .await;
    }
    rig
}

async fn state(rig: &Rig) -> OrgState {
    guarded(rig.org.snapshot()).await.unwrap()
}

fn tags(list: &[&str]) -> Vec<String> {
    list.iter().map(|t| (*t).to_owned()).collect()
}

fn ids(projects: &[(ProjectId, PathBuf)]) -> Vec<String> {
    projects.iter().map(|(id, _)| id.to_string()).collect()
}

fn demo() -> PostTarget {
    PostTarget::Channel(ChannelId::Project(project("demo")))
}

async fn read(projects: Vec<(ProjectId, PathBuf)>) -> RoleDirectory {
    guarded(role_directory(Arc::new(Semaphore::new(1)), projects)).await
}

// Break: reading for every post, or for `@owner`, which no role can be.
#[tokio::test]
async fn directory_projects_reads_nothing_without_tokens() {
    let (d, a) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let rig = rooted(d.path(), a.path()).await;
    let state = state(&rig).await;
    assert!(directory_projects(&state, &demo(), "no tags here", &[]).is_empty());
    assert!(directory_projects(&state, &demo(), "ask @owner", &tags(&["owner"])).is_empty());
    assert!(directory_projects(&state, &demo(), "mail user@host", &[]).is_empty());
}

// Break: a long form read from the channel's project, a strict tag's project
// skipped, or an unregistered project read from a guessed root.
#[tokio::test]
async fn directory_projects_names_the_scope_and_every_long_form() {
    let (d, a) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let rig = rooted(d.path(), a.path()).await;
    let state = state(&rig).await;
    let both = directory_projects(&state, &demo(), "@builders", &tags(&["acme/builders"]));
    assert_eq!(ids(&both), ["acme", "demo"]);
    assert_eq!(both[0].1, a.path());
    assert_eq!(both[1].1, d.path());
    let inline = directory_projects(&state, &demo(), "@acme/x and @zzz/y", &[]);
    assert_eq!(ids(&inline), ["acme"]);
    let strict = directory_projects(&state, &demo(), "", &tags(&["builders"]));
    assert_eq!(ids(&strict), ["demo"]);
}

// Break: a reply's short form scoped to nothing, or to a direct root's party.
#[tokio::test]
async fn directory_projects_of_a_reply_is_its_roots_project() {
    let (d, a) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let rig = rooted(d.path(), a.path()).await;
    let mut ids_of = Vec::new();
    for params in [
        json!({"channel":"demo","body":"root"}),
        json!({"to":"demo/worker","body":"root"}),
    ] {
        let p: ChannelPostParams = serde_json::from_value(params).unwrap();
        let result = guarded(rig.messages.post(p, owner())).await.unwrap();
        ids_of.push(result.post_id);
    }
    let state = state(&rig).await;
    let reply = |id: MessageId| PostTarget::Reply(id);
    assert_eq!(
        ids(&directory_projects(
            &state,
            &reply(ids_of[0]),
            "@builders",
            &[]
        )),
        ["demo"]
    );
    assert!(directory_projects(&state, &reply(ids_of[1]), "@builders", &[]).is_empty());
}

// Break: a direct channel's short name read as the author's project's group.
#[tokio::test]
async fn directory_projects_of_a_direct_channel_reads_only_long_forms() {
    let (d, a) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let rig = rooted(d.path(), a.path()).await;
    let state = state(&rig).await;
    let direct = PostTarget::Direct(Address::Agent(seat("demo/worker")));
    let projects = directory_projects(&state, &direct, "@builders @acme/builders", &tags(&["x"]));
    assert_eq!(ids(&projects), ["acme"]);
}

// Break: members in listing order, a broken role file forgotten (it is still a
// seat with a role file), or a file that is no role read as one.
#[tokio::test]
async fn read_lists_groups_and_seats_in_name_order() {
    let root = tempfile::tempdir().unwrap();
    role(root.path(), "b1", &["builders"]);
    role(root.path(), "a1", &["builders", "hard"]);
    role(root.path(), "plain", &[]);
    let roles = root.path().join(".atm/roles");
    fs::write(
        roles.join("broken.md"),
        "+++\nharness = 'pi'\ntyop = 1\n+++\n",
    )
    .unwrap();
    fs::write(roles.join("notes.txt"), "not a role").unwrap();
    fs::write(roles.join("Bad_Name.md"), "+++\nharness = 'pi'\n+++\n").unwrap();
    let directory = read(vec![(project("demo"), root.path().to_owned())]).await;
    let builders = directory.groups.get(&(project("demo"), "builders".into()));
    assert_eq!(builders, Some(&vec![seat("demo/a1"), seat("demo/b1")]));
    let hard = directory.groups.get(&(project("demo"), "hard".into()));
    assert_eq!(hard, Some(&vec![seat("demo/a1")]));
    assert_eq!(directory.groups.len(), 2);
    let seats: BTreeSet<_> = ["demo/a1", "demo/b1", "demo/broken", "demo/plain"]
        .into_iter()
        .map(seat)
        .collect();
    assert_eq!(directory.seats, seats);
    assert!(directory.problems.is_empty(), "{:?}", directory.problems);
}

// Break: a project with no roles folder reported as a problem.
#[tokio::test]
async fn missing_roles_directory_is_empty_without_a_problem() {
    let root = tempfile::tempdir().unwrap();
    let directory = read(vec![(project("demo"), root.path().to_owned())]).await;
    assert!(directory.groups.is_empty() && directory.seats.is_empty());
    assert!(directory.problems.is_empty(), "{:?}", directory.problems);
}

// Break: one unreadable project drops the others, or passes silently.
#[tokio::test]
async fn unreadable_roles_path_is_a_problem_for_that_project_only() {
    let (bad, good) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::create_dir_all(bad.path().join(".atm")).unwrap();
    fs::write(bad.path().join(".atm/roles"), "a file, not a folder").unwrap();
    role(good.path(), "b1", &["builders"]);
    let directory = read(vec![
        (project("acme"), good.path().to_owned()),
        (project("demo"), bad.path().to_owned()),
        (project("rel"), PathBuf::from("relative/root")),
    ])
    .await;
    assert_eq!(directory.seats, BTreeSet::from([seat("acme/b1")]));
    assert_eq!(directory.problems.len(), 2, "{:?}", directory.problems);
    assert!(
        directory.problems[0].starts_with("demo: "),
        "{:?}",
        directory.problems
    );
    assert!(
        directory.problems[1].starts_with("rel: "),
        "{:?}",
        directory.problems
    );
}

// Break: a busy reader waited on past the file budget, or failing the post.
#[tokio::test(start_paused = true)]
async fn busy_reader_gives_an_empty_directory_that_says_why() {
    let slot = Arc::new(Semaphore::new(1));
    let _held = slot.clone().acquire_owned().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    role(root.path(), "b1", &["builders"]);
    let projects = vec![(project("demo"), root.path().to_owned())];
    let directory = guarded(role_directory(slot, projects)).await;
    assert!(directory.groups.is_empty() && directory.seats.is_empty());
    assert_eq!(directory.problems.len(), 1);
    assert!(
        directory.problems[0].contains("longer than 5 s"),
        "{:?}",
        directory.problems
    );
}
