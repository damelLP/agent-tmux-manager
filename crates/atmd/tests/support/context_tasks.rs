//! E1's task field uses the same real task methods and committed state.
use super::*;
async fn task_call(rig: &Rig, pane: Option<&str>, method: &str, params: Value) -> Value {
    rig.router
        .call(
            CallContext {
                client_id: CallerId("context-tasks".into()),
                from_pane: pane.map(|s| CallerPane(s.into())),
            },
            method,
            params,
        )
        .await
        .unwrap()
}
async fn make(
    rig: &Rig,
    title: &str,
    agent: Option<&str>,
    state: &str,
    creator: Option<&str>,
) -> String {
    let result = task_call(
        rig,
        creator,
        "task.create",
        json!({"title":title,"measure":{"kind":"mechanical","command":"check"}}),
    )
    .await;
    let id = result["task_id"].as_str().unwrap().to_owned();
    if let Some(agent) = agent {
        task_call(
            rig,
            None,
            "task.assign",
            json!({"task_id":id,"agent_id":agent}),
        )
        .await;
    }
    match state {
        "open" | "assigned" => (),
        "submitted" | "accepted" | "changes_requested" | "in_review" => {
            task_call(rig, Some("%7"), "task.submit", json!({"task_id":id})).await;
            match state {
                "accepted" | "changes_requested" => {
                    task_call(
                        rig,
                        None,
                        "task.verdict",
                        json!({"task_id":id,"verdict":state}),
                    )
                    .await;
                }
                "in_review" => {
                    let review=task_call(rig,None,"task.create",json!({"title":"Independent review","kind":"review","judges":id,"measure":{"kind":"owner"}})).await;
                    task_call(
                        rig,
                        None,
                        "task.review",
                        json!({"task_id":id,"review_task":review["task_id"]}),
                    )
                    .await;
                }
                _ => (),
            }
        }
        "cancelled" => {
            task_call(
                rig,
                None,
                "task.set_state",
                json!({"task_id":id,"state":"cancelled"}),
            )
            .await;
        }
        _ => {
            task_call(
                rig,
                Some("%7"),
                "task.set_state",
                json!({"task_id":id,"state":state,"reason":"needs input"}),
            )
            .await;
        }
    }
    id
}

// Break caught: creator-only/other/final tasks leak into me, or submitted/in-review disappear.
#[tokio::test]
async fn only_callers_assigned_nonfinal_tasks_appear_in_creation_order_without_writes() {
    let rig = Rig::new().await;
    rig.org
        .execute(
            OrgCommand::Adopt {
                agent_id: "project/other".parse().unwrap(),
                pane: "%8".into(),
                session_id: None,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    make(&rig, "Unassigned", None, "open", None).await;
    make(
        &rig,
        "Created by caller",
        Some("project/other"),
        "assigned",
        Some("%7"),
    )
    .await;
    make(&rig, "Accepted", Some("project/worker"), "accepted", None).await;
    make(&rig, "Cancelled", Some("project/worker"), "cancelled", None).await;
    let mut ids = Vec::new();
    for state in [
        "assigned",
        "working",
        "waiting",
        "blocked",
        "submitted",
        "in_review",
        "changes_requested",
    ] {
        ids.push(
            make(
                &rig,
                &format!("{state} é🙂"),
                Some("project/worker"),
                state,
                None,
            )
            .await,
        );
    }
    let before = rig.org.snapshot().await.unwrap();
    let cursor = rig.org.status().unwrap();
    let context = rig.context().await;
    let got = context["open_tasks"].as_array().unwrap();
    assert_eq!(got.len(), 7);
    assert_eq!(
        got.iter()
            .map(|t| t["task_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ids.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(
        got.iter()
            .map(|t| t["state"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "assigned",
            "working",
            "waiting",
            "blocked",
            "submitted",
            "in_review",
            "changes_requested"
        ]
    );
    for task in got {
        assert_eq!(task["agent_id"], "project/worker");
        assert_eq!(task["kind"], "work");
    }
    assert_eq!(rig.context().await, context);
    assert_eq!(rig.org.snapshot().await.unwrap(), before);
    assert_eq!(rig.org.status().unwrap(), cursor);
}

// Break caught: context size excludes task entries or silently truncates pending work.
#[tokio::test]
async fn context_byte_budget_includes_tasks_and_explains_both_read_paths() {
    let rig = Rig::new().await;
    for _ in 0..33 {
        let id = make(&rig, "Waiting", Some("project/worker"), "assigned", None).await;
        task_call(
            &rig,
            Some("%7"),
            "task.set_state",
            json!({"task_id":id,"state":"waiting","reason":"x".repeat(16384)}),
        )
        .await;
    }
    let error = rig.call(Some("%7"), json!({})).await.unwrap_err();
    assert_eq!(error.0.code, "refused");
    assert!(error.0.message.contains("524288"));
    assert!(error.0.message.contains("atm task list"));
    assert!(error.0.message.contains("atm inbox --unread"));
}
