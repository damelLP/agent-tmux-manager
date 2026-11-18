//! Synthetic multi-project report RPCs over a real private organisation log.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::org::{
    decisions::{spawn_decisions, DecisionHandle},
    messages::MessageClock,
    OrgHandle,
};
use crate::server::methods::{
    router, CallContext, CallerId, CallerPane, MethodRouter, MethodServices,
};
use atm_org::*;
use serde_json::{json, Value};
use std::{future::Future, time::Duration};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(super) async fn guarded<T>(work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .expect("report test hang guard")
}
pub(super) fn owner() -> CallContext {
    CallContext {
        client_id: CallerId("digest-test".into()),
        from_pane: None,
    }
}
pub(super) fn agent() -> CallContext {
    CallContext {
        client_id: CallerId("digest-test".into()),
        from_pane: Some(CallerPane("%7".into())),
    }
}
pub(super) async fn setup() -> (
    tempfile::TempDir,
    OrgHandle,
    MethodRouter,
    CancellationToken,
    JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
    for (index, name) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
        let root = dir.path().join(name);
        std::fs::create_dir_all(root.join(".atm")).unwrap();
        if name != "gamma" {
            assert!(root.canonicalize().unwrap().starts_with(dir.path()));
            let git = || {
                let mut git = tokio::process::Command::new("git");
                git.env_clear()
                    .env("PATH", std::env::var_os("PATH").unwrap())
                    .env("HOME", dir.path())
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .arg("--git-dir")
                    .arg(root.join(".git"))
                    .arg("--work-tree")
                    .arg(&root);
                git
            };
            assert!(guarded(git().arg("init").output())
                .await
                .unwrap()
                .status
                .success());
            let probe = guarded(git().args(["rev-parse", "--absolute-git-dir"]).output())
                .await
                .unwrap();
            assert!(probe.status.success());
            assert_eq!(
                std::path::Path::new(String::from_utf8(probe.stdout).unwrap().trim()),
                root.join(".git")
            );
        }
        std::fs::write(
            root.join(".atm/away.toml"),
            format!("stacking_cap = {}\n", index + 1),
        )
        .unwrap();
        org.execute(
            OrgCommand::RegisterProject {
                project_id: name.parse().unwrap(),
                root: root.to_str().unwrap().into(),
                move_root: false,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        let seat = if name == "gamma" { "chief" } else { "lead" };
        let id: AgentId = format!("{name}/{seat}").parse().unwrap();
        let manager = if name == "gamma" {
            "owner"
        } else {
            "gamma/chief"
        };
        let role = parse_role(
            seat,
            &format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nWork"),
        )
        .unwrap();
        org.execute(
            OrgCommand::Up {
                agent_id: id.clone(),
                role,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        org.execute(
            OrgCommand::PaneCreated {
                agent_id: id,
                pane: format!("%{}", index + 7),
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }
    let (handle, bridge) = spawn_decisions(
        org.clone(),
        MessageClock::new(tokio::time::Instant::now(), chrono::Utc::now()),
        cancel.clone(),
    );
    guarded(handle.initialized()).await.unwrap();
    let methods = methods(&org, handle);
    (dir, org, methods, cancel, bridge)
}
pub(super) fn methods(org: &OrgHandle, handle: DecisionHandle) -> MethodRouter {
    let mut services = MethodServices::new(crate::registry::spawn_registry());
    services.org = org.clone();
    services.decisions = Some(handle);
    router(services).unwrap()
}

// Break caught: report pages leak to a bound manager, accept invalid cursors, or mix snapshots.
#[tokio::test]
async fn return_digest_pages_are_owner_only_strict_and_snapshot_bound() {
    let (_dir, org, methods, cancel, bridge) = setup().await;
    let back = guarded(methods.call(owner(), "owner.back", json!({})))
        .await
        .unwrap();
    let seq = back["digest"]["snapshot_seq"].clone();
    assert_eq!(
        guarded(methods.call(agent(), "owner.digest", json!({"snapshot_seq":seq})))
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
    for params in [
        json!({}),
        json!({"snapshot_seq":seq,"limit":0}),
        json!({"snapshot_seq":seq,"limit":1001}),
        json!({"snapshot_seq":seq,"after":999}),
        json!({"snapshot_seq":seq,"by":"owner"}),
    ] {
        assert_eq!(
            guarded(methods.call(owner(), "owner.digest", params))
                .await
                .unwrap_err()
                .0
                .code,
            "invalid_params"
        );
    }
    guarded(methods.call(owner(), "owner.back", json!({})))
        .await
        .unwrap();
    assert_eq!(
        guarded(methods.call(owner(), "owner.digest", json!({"snapshot_seq":seq})))
            .await
            .unwrap_err()
            .0
            .code,
        "conflict"
    );
    assert_eq!(org.snapshot().await.unwrap().owner().away_since, None);
    let disabled = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    assert_eq!(
        guarded(disabled.call(owner(), "owner.digest", json!({"snapshot_seq":1})))
            .await
            .unwrap_err()
            .0
            .code,
        "unavailable"
    );
    for limit in [0, 1001] {
        assert_eq!(
            guarded(disabled.call(
                owner(),
                "owner.digest",
                json!({"snapshot_seq":1,"limit":limit})
            ))
            .await
            .unwrap_err()
            .0
            .code,
            "invalid_params"
        );
    }
    cancel.cancel();
    guarded(bridge).await.unwrap();
}

// Break caught: Back/report gap loses the completed absence, or a new away is ignored.
#[tokio::test]
async fn back_then_report_gap_recovers_closed_absence() {
    let (dir, org, methods, cancel, bridge) = setup().await;
    guarded(methods.call(owner(), "owner.away", json!({})))
        .await
        .unwrap();
    org.execute(
        OrgCommand::Owner(OwnerCommand::Back { by: Address::Owner }),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    assert!(org.snapshot().await.unwrap().return_digest().is_none());
    cancel.cancel();
    guarded(bridge).await.unwrap();
    let recovered = tempfile::tempdir().unwrap();
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dest = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), dest).unwrap();
            }
        }
    }
    copy(&dir.path().join("state"), &recovered.path().join("state"));
    let cancel = CancellationToken::new();
    let org = OrgHandle::open(recovered.path().join("state"), cancel.clone()).await;
    let (handle, bridge) = spawn_decisions(
        org.clone(),
        MessageClock::new(tokio::time::Instant::now(), chrono::Utc::now()),
        cancel.clone(),
    );
    guarded(handle.initialized()).await.unwrap();
    let methods = self::methods(&org, handle);
    let back = guarded(methods.call(owner(), "owner.back", json!({})))
        .await
        .unwrap();
    assert_eq!(back["changed"], false);
    assert!(back["digest"]["absence"].is_object());
    assert_eq!(back["away_since"], Value::Null);
    let closed = back["digest"]["absence"]["back_seq"].as_u64().unwrap();
    guarded(methods.call(owner(), "owner.away", json!({})))
        .await
        .unwrap();
    let error = org
        .execute(
            OrgCommand::Owner(OwnerCommand::CreateDigest {
                by: Address::Owner,
                back_seq: Some(closed),
            }),
            chrono::Utc::now(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("became away before the digest"));
    cancel.cancel();
    guarded(bridge).await.unwrap();
}

pub(super) async fn taken(org: &OrgHandle, methods: &MethodRouter) -> DecisionId {
    let mut watch = org
        .watch(
            vec!["decision.taken_provisionally".into()],
            org.status().unwrap(),
        )
        .unwrap();
    let away = guarded(methods.call(owner(), "owner.away", json!({})))
        .await
        .unwrap();
    assert_eq!(away["stacking_caps"], json!({"alpha":1,"beta":2,"gamma":3}));
    let request=guarded(methods.call(agent(),"decision.request",json!({"kind":"plan","title":"Choose records","options":["small","large"],"recommended":"small","may_be_undone":true,"changes_intent":false}))).await.unwrap();
    let id: DecisionId = request["decision_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        guarded(watch.next()).await.unwrap().unwrap().body["decision_id"],
        id.to_string()
    );
    id
}
pub(super) async fn task(org: &OrgHandle, id: DecisionId, assignee: Option<&str>) -> TaskId {
    let events = org
        .execute(
            OrgCommand::Task(TaskCommand::Create(TaskCreated {
                project_id: None,
                title: "Build records".into(),
                kind: TaskKind::Work,
                creator: Address::Owner,
                parent: None,
                judges: None,
                measure: TaskMeasure::Owner,
                budget: None,
                files: vec![],
                rests_on: Some(id),
            })),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let task_id: TaskId = format!("t{}", events.first().unwrap().seq).parse().unwrap();
    if let Some(agent_id) = assignee {
        org.execute(
            OrgCommand::Task(TaskCommand::Assign {
                task_id: task_id.clone(),
                agent_id: agent_id.parse().unwrap(),
                by: TaskActor {
                    who: Address::Owner,
                    manager_of: None,
                },
            }),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    }
    task_id
}

// Break caught: redo is unbounded, visible to the requester, or recomputed while paging.
#[tokio::test]
async fn return_digest_redo_pages_keep_required_ids_and_do_not_widen_agent_get() {
    let (_dir, org, methods, cancel, bridge) = setup().await;
    let id = taken(&org, &methods).await;
    let mut ids = vec![];
    for _ in 0..3 {
        ids.push(task(&org, id, Some("beta/lead")).await);
    }
    let back = guarded(methods.call(owner(), "owner.back", json!({})))
        .await
        .unwrap();
    assert_eq!(back["digest"]["counts"]["during_absence"], 1);
    let resolved = guarded(methods.call(
        owner(),
        "decision.resolve",
        json!({"id":id,"option":"large"}),
    ))
    .await
    .unwrap();
    assert_eq!(resolved["redo"]["required_count"], 3);
    assert_eq!(resolved["redo"]["completed_count"], 0);
    assert_eq!(resolved["redo"]["task_ids"], json!(ids));
    let page = guarded(methods.call(owner(), "decision.get", json!({"id":id,"redo_limit":1})))
        .await
        .unwrap();
    assert_eq!(page["redo"]["task_ids"], json!([ids.first().unwrap()]));
    let next = guarded(methods.call(
        owner(),
        "decision.get",
        json!({"id":id,"redo_after":page["redo"]["next_after"],"redo_limit":1}),
    ))
    .await
    .unwrap();
    assert_eq!(next["redo"]["task_ids"], json!([ids.get(1).unwrap()]));
    for params in [
        json!({"id":id,"redo_limit":0}),
        json!({"id":id,"redo_after":"t01"}),
        json!({"id":id,"redo_after":"t999999"}),
    ] {
        assert_eq!(
            guarded(methods.call(owner(), "decision.get", params))
                .await
                .unwrap_err()
                .0
                .code,
            "invalid_params"
        );
    }
    assert!(
        guarded(methods.call(agent(), "decision.get", json!({"id":id})))
            .await
            .unwrap()
            .get("redo")
            .is_none()
    );
    assert_eq!(
        guarded(methods.call(agent(), "decision.get", json!({"id":id,"redo_limit":1})))
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
    let old = guarded(methods.call(
        owner(),
        "owner.digest",
        json!({"snapshot_seq":back["digest"]["snapshot_seq"]}),
    ))
    .await
    .unwrap();
    assert_eq!(
        old, back["digest"],
        "ordinary activity must not change the frozen snapshot"
    );
    cancel.cancel();
    guarded(bridge).await.unwrap();
}

// Break caught: a rule-file retry resets reassigned redo work or writes the assignee's project.
#[tokio::test]
async fn return_digest_overrule_rule_retry_repairs_only_the_requesters_file() {
    let (dir, org, methods, cancel, bridge) = setup().await;
    let id = taken(&org, &methods).await;
    let task = task(&org, id, Some("beta/lead")).await;
    let rules = dir.path().join("alpha/.atm/rules.md");
    std::fs::create_dir(&rules).unwrap();
    let params = json!({"id":id,"option":"large","standing_rule":true});
    let error = guarded(methods.call(owner(), "decision.resolve", params.clone()))
        .await
        .unwrap_err();
    assert!(error.0.message.contains("committed"));
    assert_eq!(
        org.snapshot().await.unwrap().task(&task).unwrap().state,
        TaskState::Open
    );
    org.execute(
        OrgCommand::Task(TaskCommand::Assign {
            task_id: task.clone(),
            agent_id: "gamma/chief".parse().unwrap(),
            by: TaskActor {
                who: Address::Owner,
                manager_of: None,
            },
        }),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    std::fs::remove_dir(&rules).unwrap();
    let result = guarded(methods.call(owner(), "decision.resolve", params))
        .await
        .unwrap();
    assert_eq!(result["changed"], false);
    assert_eq!(result["redo"]["task_ids"], json!([task]));
    assert_eq!(
        org.snapshot().await.unwrap().task(&task).unwrap().state,
        TaskState::Assigned
    );
    assert!(std::fs::read_to_string(rules)
        .unwrap()
        .contains(&id.to_string()));
    assert!(!dir.path().join("beta/.atm/rules.md").exists());
    cancel.cancel();
    guarded(bridge).await.unwrap();
}

// Break caught: adding redo IDs exceeds the full decision result budget.
#[test]
fn return_digest_redo_budget_includes_the_existing_decision() {
    let mut state = OrgState::default();
    for line in include_str!("../../../atm-org/tests/fixtures/return_digest.jsonl").lines() {
        let mut value: Value = serde_json::from_str(line).unwrap();
        if value["kind"] == "decision.requested" {
            value["body"]["context"] = json!("x".repeat(super::PAGE_BYTES - 1024));
        }
        let event = OrgEvent::from_parts(value["kind"].as_str().unwrap(), &value["body"])
            .unwrap()
            .unwrap();
        state
            .apply(
                value["seq"].as_u64().unwrap(),
                value["ts"].as_str().unwrap().parse().unwrap(),
                &event,
            )
            .unwrap();
    }
    let now = chrono::Utc::now();
    let mut seq = 31;
    for _ in 0..140 {
        seq += 1;
        let id: TaskId = format!("t{seq}").parse().unwrap();
        state
            .apply(
                seq,
                now,
                &OrgEvent::TaskCreated(TaskCreated {
                    project_id: None,
                    title: "Work".into(),
                    kind: TaskKind::Work,
                    creator: Address::Owner,
                    parent: None,
                    judges: None,
                    measure: TaskMeasure::Owner,
                    budget: None,
                    files: vec![],
                    rests_on: Some(DecisionId::new(11)),
                }),
            )
            .unwrap();
        seq += 1;
        state
            .apply(
                seq,
                now,
                &OrgEvent::TaskAssigned(TaskAssigned {
                    task_id: id,
                    agent_id: "alpha/builder".parse().unwrap(),
                    by: Address::Owner,
                }),
            )
            .unwrap();
    }
    let events = state
        .decide(
            &OrgCommand::Decision(DecisionCommand::Resolve(DecisionResolution {
                decision_id: DecisionId::new(11),
                option: "large".into(),
                by: Address::Owner,
                note: None,
                standing_rule: false,
            })),
            now,
        )
        .unwrap();
    state.apply(seq + 1, now, events.first().unwrap()).unwrap();
    let mut result = crate::org::decisions::GetResult {
        decision: state.decision(&DecisionId::new(11)).unwrap().into(),
        redo: None,
    };
    result.redo = Some(super::redo_page(&state, DecisionId::new(11), None, 100, &result).unwrap());
    assert!(result.redo.as_ref().unwrap().next_after.is_some());
    assert!(serde_json::to_vec(&result).unwrap().len() <= 524288);
    let params = crate::org::decisions::GetParams {
        id: DecisionId::new(11),
        redo_after: None,
        redo_limit: None,
    };
    assert_eq!(serde_json::to_value(params).unwrap(), json!({"id":"d11"}));
}

// Break caught: an oversized legacy answer is reported as an uncommitted refusal.
#[tokio::test]
async fn return_digest_oversized_resolution_reports_its_commit() {
    let (_dir, org, methods, cancel, bridge) = setup().await;
    let events = org
        .execute(
            OrgCommand::Decision(DecisionCommand::Request(DecisionRequest {
                requester: "alpha/lead".parse().unwrap(),
                kind: DecisionKind::Plan,
                title: "Legacy question".into(),
                context: "x".repeat(524288),
                files: vec![],
                options: vec!["small".into(), "large".into()],
                recommended: None,
                may_be_undone: true,
                changes_intent: false,
                deadline: None,
            })),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let id = DecisionId::new(events.first().unwrap().seq);
    let error = guarded(methods.call(
        owner(),
        "decision.resolve",
        json!({"id":id,"option":"large"}),
    ))
    .await
    .unwrap_err();
    assert!(error.0.message.contains("committed"), "{}", error.0.message);
    assert_eq!(
        org.snapshot().await.unwrap().decision(&id).unwrap().status,
        DecisionStatus::Resolved
    );
    cancel.cancel();
    guarded(bridge).await.unwrap();
}

// Break caught: back only reports counts, or owner.digest is not registered.
#[tokio::test]
async fn return_digest_is_created_even_before_the_first_absence() {
    let (_dir, org, methods, cancel, bridge) = setup().await;
    let back = guarded(methods.call(owner(), "owner.back", json!({})))
        .await
        .unwrap();
    assert!(
        back["digest"].is_object(),
        "back must prepare a committed frozen report"
    );
    assert_eq!(back["changed"], false);
    assert_eq!(back["digest"]["absence"], Value::Null);
    let page = guarded(methods.call(
        owner(),
        "owner.digest",
        json!({"snapshot_seq":back["digest"]["snapshot_seq"],"limit":1}),
    ))
    .await
    .unwrap();
    assert_eq!(page["total_rows"], 0);
    assert_eq!(
        org.snapshot()
            .await
            .unwrap()
            .return_digest()
            .unwrap()
            .snapshot_seq,
        back["digest"]["snapshot_seq"].as_u64().unwrap()
    );
    cancel.cancel();
    guarded(bridge).await.unwrap();
}
