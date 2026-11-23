//! `channel.post` with role groups (unit U10, design 9.3, 9.4): groups read
//! from real role files in temp project roots, expanded when the post is
//! decided and stored with it. Every wait is on a committed or folded seq;
//! the busy-reader cases hold the role-reader slot on a paused clock.

use super::message_service_tests::{guarded, other, owner, pane, worker, Rig};
use super::role_directory_tests::role;
use crate::server::methods::{CallContext, MethodError};
use atm_comms::channel_types::{ChannelPostParams, ChannelPostResult};
use atm_org::{channel_query, Address, AgentId, Mention, MessageId, OrgCommand, ProjectId};
use atm_protocol::error_code;
use serde_json::{json, Value};
use std::path::Path;
use tempfile::TempDir;

fn lead() -> AgentId {
    "demo/lead".parse().unwrap()
}

fn group(project: &str, name: &str) -> Mention {
    Mention::Group {
        project: project.parse().unwrap(),
        name: name.into(),
    }
}

async fn register(rig: &Rig, id: &str, root: &Path, move_root: bool) {
    rig.run(OrgCommand::RegisterProject {
        project_id: id.parse::<ProjectId>().unwrap(),
        root: root.to_str().unwrap().into(),
        move_root,
    })
    .await;
}

async fn adopt(rig: &Rig, id: AgentId, pane: &str, session: &str) {
    rig.run(OrgCommand::Adopt {
        agent_id: id,
        pane: pane.into(),
        session_id: Some(session.into()),
    })
    .await;
}

/// The seeded rig plus `demo/lead`, with `demo`'s root moved to a temp dir:
/// `demo/worker`, `demo/other` and `demo/lead` are live.
async fn rig() -> (Rig, TempDir) {
    let rig = Rig::seeded().await;
    adopt(&rig, lead(), "%3", "s3").await;
    let root = tempfile::tempdir().unwrap();
    register(&rig, "demo", root.path(), true).await;
    (rig, root)
}

async fn down(rig: &Rig, id: AgentId) {
    rig.run(OrgCommand::Down {
        agent_id: id,
        reason: "fixture".into(),
    })
    .await;
}

async fn post(rig: &Rig, caller: CallContext, params: Value) -> Result<Value, MethodError> {
    let p: ChannelPostParams = serde_json::from_value(params).unwrap();
    guarded(rig.messages.post(p, caller))
        .await
        .map(|result: ChannelPostResult| serde_json::to_value(result).unwrap())
}

fn woke(result: &Value) -> Vec<&str> {
    result["woke"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["to"].as_str().unwrap())
        .collect()
}

fn post_id(result: &Value) -> MessageId {
    serde_json::from_value(result["post_id"].clone()).unwrap()
}

/// The last committed seq, to show a refusal wrote nothing.
async fn head(rig: &Rig) -> u64 {
    guarded(rig.org.snapshot_at()).await.unwrap().1
}

/// A refusal: its code, its text, and that nothing was written.
async fn refused(rig: &Rig, caller: CallContext, params: Value) -> String {
    let before = head(rig).await;
    let error = post(rig, caller, params).await.unwrap_err();
    assert_eq!(error.0.code, error_code::INVALID_PARAMS, "{error}");
    assert_eq!(head(rig).await, before, "a refused post wrote records");
    error.0.message
}

// Break: a group unread from the role files, a down member woken, the
// expansion not stored, or history rewritten when a role file changes.
#[tokio::test]
async fn group_tag_wakes_its_live_members_and_is_stored_with_the_post() {
    let (rig, root) = rig().await;
    role(root.path(), "worker", &["builders"]);
    role(root.path(), "other", &["builders"]);
    role(root.path(), "lead", &["builders"]);
    role(root.path(), "ghost", &["builders"]); // a role file never started
    down(&rig, other()).await;
    let strict = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"go","tags":["builders"]}),
    )
    .await
    .unwrap();
    assert_eq!(woke(&strict), ["demo/lead", "demo/worker"]);
    assert_eq!(
        strict["mentions"],
        json!([{"group":{"project":"demo","name":"builders"}}])
    );
    let inline = post(
        &rig,
        pane("%3"),
        json!({"channel":"demo","body":"@builders go"}),
    )
    .await
    .unwrap();
    assert_eq!(woke(&inline), ["demo/worker"], "the author is not woken");
    role(root.path(), "worker", &[]);
    role(root.path(), "other", &[]);
    let state = guarded(rig.org.snapshot()).await.unwrap();
    for (result, wake) in [
        (&strict, vec!["demo/lead", "demo/worker"]),
        (&inline, vec!["demo/worker"]),
    ] {
        let stored = channel_query::post(&state, post_id(result)).unwrap();
        assert_eq!(stored.content.mentions, [group("demo", "builders")]);
        let stored_wake: Vec<String> = stored.woke().map(Address::to_string).collect();
        assert_eq!(stored_wake, wake);
    }
}

// Break: a long form read from the channel's project, or refused outside its
// own project's channel.
#[tokio::test]
async fn long_form_group_resolves_from_another_projects_channel_and_a_direct_channel() {
    let (rig, _demo) = rig().await;
    let acme = tempfile::tempdir().unwrap();
    register(&rig, "acme", acme.path(), false).await;
    let builder: AgentId = "acme/b1".parse().unwrap();
    adopt(&rig, builder, "%5", "s5").await;
    role(acme.path(), "b1", &["builders"]);
    let from_demo = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"x","tags":["acme/builders"]}),
    )
    .await
    .unwrap();
    assert_eq!(woke(&from_demo), ["acme/b1"]);
    assert_eq!(
        from_demo["mentions"],
        json!([{"group":{"project":"acme","name":"builders"}}])
    );
    let direct = post(
        &rig,
        owner(),
        json!({"to":"demo/lead","body":"ask @acme/builders"}),
    )
    .await
    .unwrap();
    assert_eq!(woke(&direct), ["demo/lead", "acme/b1"]);
    assert_eq!(direct["unresolved"], json!([]));
}

// Break: an empty group accepted when strict, or failing the post inline.
#[tokio::test]
async fn empty_group_is_refused_when_strict_and_reported_when_inline() {
    let (rig, root) = rig().await;
    role(root.path(), "other", &["builders"]);
    down(&rig, other()).await;
    let text = refused(
        &rig,
        owner(),
        json!({"channel":"demo","body":"x","tags":["builders"]}),
    )
    .await;
    assert!(
        text.contains("\"builders\"") && text.contains("EmptyGroup"),
        "{text}"
    );
    let inline = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"@builders x"}),
    )
    .await
    .unwrap();
    assert_eq!(
        inline["unresolved"],
        json!([{"text":"@builders","reason":"empty_group"}])
    );
    assert_eq!(inline["mentions"], json!([]));
}

// Break: a group shadowing the seat whose name it shares.
#[tokio::test]
async fn seat_wins_over_a_group_of_the_same_name() {
    let (rig, root) = rig().await;
    role(root.path(), "other", &["worker"]);
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"@worker","tags":["worker"]}),
    )
    .await
    .unwrap();
    assert_eq!(woke(&result), ["demo/worker"]);
    assert_eq!(result["mentions"], json!([{"seat":"demo/worker"}]));
}

// Break: the result's unresolved list computed without the post's groups.
#[tokio::test]
async fn inline_group_that_resolved_is_not_listed_unresolved() {
    let (rig, root) = rig().await;
    role(root.path(), "worker", &["builders"]);
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"@builders x"}),
    )
    .await
    .unwrap();
    assert_eq!(woke(&result), ["demo/worker"]);
    assert_eq!(result["unresolved"], json!([]));
}

/// Hold the shared role-reader slot, as a hung read would.
fn hold_slot(rig: &Rig) -> tokio::sync::OwnedSemaphorePermit {
    let service = rig.messages.service().unwrap();
    service.notice_files.clone().try_acquire_owned().unwrap()
}

// Break: a busy reader failing every post, or a refusal that hides why.
#[tokio::test(start_paused = true)]
async fn busy_role_reader_refuses_a_strict_group_tag_and_says_why() {
    let (rig, root) = rig().await;
    role(root.path(), "worker", &["builders"]);
    let _held = hold_slot(&rig);
    let text = refused(
        &rig,
        owner(),
        json!({"channel":"demo","body":"x","tags":["builders"]}),
    )
    .await;
    assert!(text.contains("role files could not be read"), "{text}");
    assert!(text.contains("longer than 5 s"), "{text}");
}

// Break: a busy reader failing a post whose tags are only text.
#[tokio::test(start_paused = true)]
async fn busy_role_reader_lets_an_inline_post_through_unresolved() {
    let (rig, root) = rig().await;
    role(root.path(), "worker", &["builders"]);
    let _held = hold_slot(&rig);
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"@builders x"}),
    )
    .await
    .unwrap();
    assert_eq!(
        result["unresolved"],
        json!([{"text":"@builders","reason":"unknown"}])
    );
}

// Break: a post with no tag waits on the role reader. On a paused clock the
// wait is the 5 s budget exactly; with no wait the clock does not move.
#[tokio::test(start_paused = true)]
async fn untagged_post_never_waits_for_the_role_reader() {
    let (rig, _root) = rig().await;
    let _held = hold_slot(&rig);
    let start = tokio::time::Instant::now();
    post(&rig, owner(), json!({"channel":"demo","body":"no tags"}))
        .await
        .unwrap();
    assert_eq!(tokio::time::Instant::now(), start);
}

// Break: a role file without a record reported as an unknown name, the
// short form read outside its scope, or the refusal writing anything.
#[tokio::test]
async fn strict_tag_of_a_never_started_seat_is_refused_naming_it() {
    let (rig, root) = rig().await;
    role(root.path(), "ghost", &[]);
    let started = "has a role file but has never been started";
    let short = refused(
        &rig,
        owner(),
        json!({"channel":"demo","body":"x","tags":["ghost"]}),
    )
    .await;
    assert!(
        short.contains("\"ghost\"") && short.contains(&format!("demo/ghost {started}")),
        "{short}"
    );
    let long = refused(
        &rig,
        owner(),
        json!({"to":"demo/worker","body":"x","tags":["demo/ghost"]}),
    )
    .await;
    assert!(long.contains(&format!("demo/ghost {started}")), "{long}");
    let root_post = post(&rig, owner(), json!({"channel":"demo","body":"root"}))
        .await
        .unwrap();
    let reply = json!({"reply_to":root_post["post_id"],"body":"x","tags":["ghost"]});
    let in_thread = refused(&rig, owner(), reply).await;
    assert!(
        in_thread.contains(&format!("demo/ghost {started}")),
        "{in_thread}"
    );
}

// Break: the never-started text given to a name with no role file.
#[tokio::test]
async fn strict_tag_without_a_role_file_keeps_the_unknown_text() {
    let (rig, _root) = rig().await;
    let text = refused(
        &rig,
        owner(),
        json!({"channel":"demo","body":"x","tags":["ghost"]}),
    )
    .await;
    assert!(text.contains("does not resolve: Unknown"), "{text}");
    assert!(!text.contains("never been started"), "{text}");
    assert!(!text.contains("could not be read"), "{text}");
}

// Break: an inline mention of a never-started seat failing the post.
#[tokio::test]
async fn inline_tag_of_a_never_started_seat_is_unresolved_and_the_post_goes_through() {
    let (rig, root) = rig().await;
    role(root.path(), "ghost", &[]);
    let result = post(&rig, owner(), json!({"channel":"demo","body":"@ghost hi"}))
        .await
        .unwrap();
    assert_eq!(
        result["unresolved"],
        json!([{"text":"@ghost","reason":"unknown"}])
    );
}

// Break: a down seat (it has a record) refused as never started instead of
// queued with the loud `unavailable` entry (design 9.4).
#[tokio::test]
async fn down_seat_with_a_role_file_is_not_never_started() {
    let (rig, root) = rig().await;
    role(root.path(), "other", &[]);
    down(&rig, other()).await;
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"x","tags":["other"]}),
    )
    .await
    .unwrap();
    assert_eq!(woke(&result), ["demo/other"]);
    assert_eq!(result["unavailable"][0]["to"], "demo/other");
    assert_eq!(result["unavailable"][0]["state"], "down");
}

// Break: a never-started role file shadowing a live group of its name (the
// lead's ruling Q1: only a seat with a record beats a group).
#[tokio::test]
async fn group_wins_over_a_never_started_role_of_the_same_name() {
    let (rig, root) = rig().await;
    role(root.path(), "builders", &[]);
    role(root.path(), "worker", &["builders"]);
    let result = post(
        &rig,
        owner(),
        json!({"channel":"demo","body":"x","tags":["builders"]}),
    )
    .await
    .unwrap();
    assert_eq!(woke(&result), ["demo/worker"]);
    // Empty, the group still answers for the name: its refusal is not "never started".
    down(&rig, worker()).await;
    let text = refused(
        &rig,
        owner(),
        json!({"channel":"demo","body":"x","tags":["builders"]}),
    )
    .await;
    assert!(
        text.contains("EmptyGroup") && !text.contains("never been started"),
        "{text}"
    );
}
