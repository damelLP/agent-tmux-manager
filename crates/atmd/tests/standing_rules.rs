//! Synthetic standing-rule commands over the actual decision method implementation.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use atm_org::{
    Address, DecisionCommand, DecisionId, DecisionRequest, OrgCommand, ProvisionalChoice,
};
use atmd::{
    org::{decisions::spawn_decisions, messages::MessageClock, OrgHandle},
    registry::spawn_registry,
    server::methods::{
        router, CallContext, CallerId, CallerPane, MethodError, MethodRouter, MethodServices,
    },
};
use serde_json::{json, Value};
use std::{fs, time::Duration};
use tokio_util::sync::CancellationToken;

struct Rig {
    dir: tempfile::TempDir,
    org: OrgHandle,
    router: MethodRouter,
    cancel: CancellationToken,
}
impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        for command in [
            OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: dir.path().to_str().unwrap().into(),
                move_root: false,
            },
            OrgCommand::Adopt {
                agent_id: "p/worker".parse().unwrap(),
                pane: "%7".into(),
                session_id: None,
            },
        ] {
            org.execute(command, chrono::Utc::now()).await.unwrap();
        }
        let (decisions, _) = spawn_decisions(
            org.clone(),
            MessageClock::new(tokio::time::Instant::now(), chrono::Utc::now()),
            cancel.child_token(),
        );
        decisions.initialized().await.unwrap();
        let mut services = MethodServices::new(spawn_registry());
        services.org = org.clone();
        services.decisions = Some(decisions);
        Self {
            dir,
            org,
            router: router(services).unwrap(),
            cancel,
        }
    }
    async fn request(&self) -> DecisionId {
        let result = self.router.call(caller(Some("%7")), "decision.request", json!({"kind":"policy","title":"Use the smaller shape?","options":["yes","no"],"may_be_undone":true,"changes_intent":false})).await.unwrap();
        serde_json::from_value(result["decision_id"].clone()).unwrap()
    }
    async fn resolve(
        &self,
        id: DecisionId,
        pane: Option<&str>,
        rule: bool,
    ) -> Result<Value, MethodError> {
        self.router
            .call(
                caller(pane),
                "decision.resolve",
                json!({"id":id,"option":"yes","note":"Always use this shape","standing_rule":rule}),
            )
            .await
    }
    fn rules(&self) -> std::path::PathBuf {
        self.dir.path().join(".atm/rules.md")
    }
    async fn resolution_count(&self) -> usize {
        self.org
            .snapshot()
            .await
            .unwrap()
            .decisions()
            .filter(|d| d.resolution.is_some())
            .count()
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn caller(pane: Option<&str>) -> CallContext {
    CallContext {
        client_id: CallerId("rule-test".into()),
        from_pane: pane.map(|p| CallerPane(p.into())),
    }
}

// Break caught: storing only the rule mark, appending before commit, or duplicate retry appends.
#[tokio::test]
async fn owner_rule_appends_the_answer_once_and_preserves_existing_notes() {
    let rig = Rig::new().await;
    fs::create_dir(rig.rules().parent().unwrap()).unwrap();
    fs::write(rig.rules(), "Owner's existing notes").unwrap();
    let id = rig.request().await;
    assert_eq!(rig.resolve(id, None, true).await.unwrap()["changed"], true);
    let first = fs::read_to_string(rig.rules()).unwrap();
    assert!(first.starts_with("Owner's existing notes\n\n"));
    assert!(first.contains("Use the smaller shape?"));
    assert!(first.contains("Answer: yes"));
    assert!(first.contains("Always use this shape"));
    assert_eq!(first.matches(&format!("<!-- atm-rule:{id} -->")).count(), 1);
    let cursor = rig.org.status().unwrap();
    assert_eq!(rig.resolve(id, None, true).await.unwrap()["changed"], false);
    assert_eq!(fs::read_to_string(rig.rules()).unwrap(), first);
    // The bridge may enqueue its answer after the resolution; inspect decision facts, not its cursor.
    assert_eq!(rig.resolution_count().await, 1);
    assert!(rig.org.status().unwrap() >= cursor);
    let second = rig.request().await;
    rig.resolve(second, None, true).await.unwrap();
    let text = fs::read_to_string(rig.rules()).unwrap();
    assert!(text.starts_with(&first));
    assert_eq!(text.matches("<!-- atm-rule:").count(), 2);
}

// Break caught: concurrent retries independently read an old file and append duplicate entries.
#[tokio::test]
async fn concurrent_identical_rule_commands_have_one_entry() {
    let rig = Rig::new().await;
    let id = rig.request().await;
    let (first, second) = tokio::join!(rig.resolve(id, None, true), rig.resolve(id, None, true));
    assert_ne!(first.unwrap()["changed"], second.unwrap()["changed"]);
    assert_eq!(
        fs::read_to_string(rig.rules())
            .unwrap()
            .matches("<!-- atm-rule:")
            .count(),
        1
    );
    assert_eq!(rig.resolution_count().await, 1);
}

// Break caught: a failed append is silently successful, or its committed resolution cannot be repaired.
#[tokio::test]
async fn failed_publication_reports_the_commit_and_identical_retry_repairs_once() {
    let rig = Rig::new().await;
    fs::create_dir(rig.rules().parent().unwrap()).unwrap();
    fs::create_dir(rig.rules()).unwrap();
    let id = rig.request().await;
    let error = rig.resolve(id, None, true).await.unwrap_err();
    assert!(error.0.message.contains("committed"), "{error}");
    assert!(error.0.message.contains("retry the identical"), "{error}");
    assert_eq!(rig.resolution_count().await, 1);
    fs::remove_dir(rig.rules()).unwrap();
    assert_eq!(rig.resolve(id, None, true).await.unwrap()["changed"], false);
    let text = fs::read_to_string(rig.rules()).unwrap();
    rig.resolve(id, None, true).await.unwrap();
    assert_eq!(fs::read_to_string(rig.rules()).unwrap(), text);
}

// Break caught: write effects for plain answers, rejected options or bound-agent callers.
#[tokio::test]
async fn only_accepted_owner_rule_commands_touch_the_rules_file() {
    let rig = Rig::new().await;
    let id = rig.request().await;
    assert_eq!(
        rig.resolve(id, Some("%7"), true).await.unwrap_err().0.code,
        "refused"
    );
    assert!(!rig.rules().exists());
    assert_eq!(
        rig.router
            .call(
                caller(None),
                "decision.resolve",
                json!({"id":id,"option":"missing","standing_rule":true})
            )
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
    assert!(!rig.rules().exists());
    rig.resolve(id, None, false).await.unwrap();
    assert!(!rig.rules().exists());
    assert_eq!(
        rig.resolve(id, None, true).await.unwrap_err().0.code,
        "refused"
    );
    assert!(!rig.rules().exists());
}

// Break caught: writing a rule for a projectless system request into the daemon/owner cwd.
#[tokio::test]
async fn system_rule_request_without_a_project_is_refused_before_commit() {
    let rig = Rig::new().await;
    let request: DecisionRequest = serde_json::from_value(json!({"requester":"system","kind":"policy","title":"System question","context":"","files":[],"options":["yes","no"],"recommended":null,"may_be_undone":true,"changes_intent":false,"deadline":null})).unwrap();
    let events = rig
        .org
        .execute(
            OrgCommand::Decision(DecisionCommand::Request(request)),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let id = DecisionId::new(events.first().unwrap().seq);
    let before = rig.org.snapshot().await.unwrap();
    assert_eq!(
        rig.resolve(id, None, true).await.unwrap_err().0.code,
        "refused"
    );
    assert_eq!(rig.org.snapshot().await.unwrap(), before);
    assert!(!rig.rules().exists());
    rig.resolve(id, None, false).await.unwrap();
}

// Break caught: appending the provisional option instead of the owner's final overrule.
#[tokio::test]
async fn overruled_rule_records_the_final_owner_answer() {
    let rig = Rig::new().await;
    let id = rig.request().await;
    // This request has no recommendation, so record the prior fact directly as a replay fixture.
    let mut request = rig
        .org
        .snapshot()
        .await
        .unwrap()
        .decision(&id)
        .unwrap()
        .request
        .clone();
    request.recommended = Some("no".into());
    let events = rig
        .org
        .execute(
            OrgCommand::Decision(DecisionCommand::Request(request)),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let id = DecisionId::new(events.first().unwrap().seq);
    rig.org
        .execute(
            OrgCommand::Decision(DecisionCommand::TakeProvisionally(ProvisionalChoice {
                decision_id: id,
                option: "no".into(),
                by: Address::System,
            })),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(
        rig.resolve(id, None, true).await.unwrap()["decision"]["status"],
        "overruled"
    );
    let text = fs::read_to_string(rig.rules()).unwrap();
    assert!(text.contains("Answer: yes"));
    assert!(!text.contains("Answer: no"));
}

// Break caught: following a rules symlink, changing permissions, or publishing an unreadable-sized file.
#[tokio::test]
async fn rule_files_are_bounded_regular_and_keep_existing_permissions() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let rig = Rig::new().await;
    fs::create_dir(rig.rules().parent().unwrap()).unwrap();
    let outside = rig.dir.path().join("outside.md");
    fs::write(&outside, "untouched").unwrap();
    symlink(&outside, rig.rules()).unwrap();
    let id = rig.request().await;
    rig.resolve(id, None, true).await.unwrap_err();
    assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
    fs::remove_file(rig.rules()).unwrap();
    fs::write(rig.rules(), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    rig.resolve(id, None, true).await.unwrap_err();
    fs::write(rig.rules(), "Owner notes\n").unwrap();
    fs::set_permissions(rig.rules(), fs::Permissions::from_mode(0o640)).unwrap();
    rig.resolve(id, None, true).await.unwrap();
    assert_eq!(
        fs::metadata(rig.rules()).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

// Break caught: rule behavior absent from actual JSON method dispatch.
#[tokio::test]
async fn standing_rule_method_fixtures() {
    let fixtures: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/standing_rules/methods.json")).unwrap();
    for fixture in fixtures {
        let rig = Rig::new().await;
        let id = rig.request().await;
        let mut params = fixture["params"].clone();
        params["id"] = json!(id);
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            rig.router
                .call(caller(fixture["pane"].as_str()), "decision.resolve", params),
        )
        .await
        .unwrap();
        if let Some(code) = fixture.get("error") {
            assert_eq!(result.unwrap_err().0.code, *code);
            assert!(!rig.rules().exists());
        } else {
            assert!(result.is_ok());
            assert_eq!(
                fs::read_to_string(rig.rules())
                    .unwrap()
                    .matches("<!-- atm-rule:")
                    .count(),
                1
            );
        }
    }
}
