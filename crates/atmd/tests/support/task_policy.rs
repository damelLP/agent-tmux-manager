//! Additional task RPC policy, filter and byte-bound contracts.
use super::*;

// Break caught: a linked review plan/self-review is counted, or its bookkeeping close is omitted.
#[tokio::test]
async fn linked_verdict_requires_a_fresh_review_result_and_closes_both_tasks() {
    for (what, reviewer, want) in [
        ("result", "project/reviewer", None),
        ("plan", "project/reviewer", Some("conflict")),
        ("result", "project/worker", Some("refused")),
    ] {
        let rig = Rig::new().await;
        rig.submitted().await;
        let review = rig
            .ok(
                None,
                "task.create",
                json!({"title":"Review","kind":"review","judges":"t6","measure":{"kind":"owner"}}),
            )
            .await;
        let id = review["task_id"].as_str().unwrap();
        rig.ok(
            None,
            "task.assign",
            json!({"task_id":id,"agent_id":reviewer}),
        )
        .await;
        let pane = if reviewer == "project/worker" {
            "%2"
        } else {
            "%3"
        };
        rig.ok(Some(pane), "task.submit", json!({"task_id":id,"what":what}))
            .await;
        let started = rig
            .call(
                Some("%1"),
                "task.review",
                json!({"task_id":"t6","review_task":id}),
            )
            .await;
        if reviewer == "project/worker" {
            assert_eq!(started.unwrap_err().0.code, "refused");
        } else {
            started.unwrap();
        }
        let before = rig.snapshot().await;
        let cursor = rig.org.status().unwrap();
        let result = rig
            .call(
                Some("%1"),
                "task.verdict",
                json!({"task_id":"t6","verdict":"accepted","review_task":id}),
            )
            .await;
        if let Some(code) = want {
            assert_eq!(result.unwrap_err().0.code, code);
            assert_eq!(rig.snapshot().await, before);
        } else {
            result.unwrap();
            let state = rig.snapshot().await;
            assert_eq!(rig.org.status().unwrap(), cursor + 2);
            for id in ["t6", id] {
                assert_eq!(
                    state.task(&id.parse().unwrap()).unwrap().state,
                    TaskState::Accepted
                );
            }
        }
    }
}

// Break caught: measure labels are ignored, or approval of a plan is treated as result acceptance.
#[tokio::test]
async fn measures_limit_acceptance_but_plan_approval_returns_to_working() {
    for (measure, lead_result) in [
        (json!({"kind":"mechanical","command":"check"}), true),
        (json!({"kind":"reviewed","rubric":"rubric.md"}), false),
        (json!({"kind":"owner"}), false),
    ] {
        for what in ["plan", "result"] {
            let rig = Rig::new().await;
            let create = rig
                .ok(
                    None,
                    "task.create",
                    json!({"title":"Measured","measure":measure}),
                )
                .await;
            let id = create["task_id"].as_str().unwrap();
            rig.ok(
                None,
                "task.assign",
                json!({"task_id":id,"agent_id":"project/worker"}),
            )
            .await;
            rig.ok(Some("%2"), "task.submit", json!({"task_id":id,"what":what}))
                .await;
            let result = rig
                .call(
                    Some("%1"),
                    "task.verdict",
                    json!({"task_id":id,"verdict":"accepted"}),
                )
                .await;
            if what == "plan" || lead_result {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().0.code, "refused");
                rig.ok(
                    None,
                    "task.verdict",
                    json!({"task_id":id,"verdict":"accepted"}),
                )
                .await;
            }
            let got = rig.ok(None, "task.get", json!({"task_id":id})).await;
            assert_eq!(
                got["task"]["state"],
                if what == "plan" {
                    "working"
                } else {
                    "accepted"
                }
            );
        }
    }
}

// Break caught: caller override accepted, filters ignored, or provisional decision/budget wire shape drift.
#[tokio::test]
async fn create_identity_budget_decision_reference_and_intersecting_filters() {
    let rig = Rig::new().await;
    let budget = json!({"cost":{"microdollars":123},"turns":0,"deadline":"2020-01-01T00:00:00Z"});
    let created=rig.ok(Some("%2"),"task.create",json!({"title":"Plan","kind":"plan","parent":"t6","measure":{"kind":"owner"},"budget":budget,"rests_on":"d99"})).await;
    let id = created["task_id"].as_str().unwrap();
    assert_eq!(id, "t7");
    rig.ok(
        Some("%2"),
        "task.assign",
        json!({"task_id":id,"agent_id":"project/worker"}),
    )
    .await;
    let got = rig.ok(None, "task.get", json!({"task_id":id})).await;
    assert_eq!(got["task"]["creator"], "project/worker");
    assert_eq!(got["task"]["budget"], budget);
    assert_eq!(got["task"]["rests_on"], "d99");
    let list=rig.ok(Some("%4"),"task.list",json!({"agent_id":"project/worker","creator":"project/worker","state":"assigned","kind":"plan","parent":"t6","open_only":true})).await;
    assert_eq!(
        list,
        json!({"tasks":[{"task_id":"t7","title":"Plan","kind":"plan","state":"assigned","agent_id":"project/worker","reason":null}],"next_after":null})
    );
    assert_eq!(
        rig.ok(None, "task.list", json!({"limit":1})).await["next_after"],
        "t6"
    );
    assert_eq!(
        rig.ok(None, "task.list", json!({"after":"t6","limit":1}))
            .await["tasks"][0]["task_id"],
        "t7"
    );
    assert_eq!(
        rig.ok(None, "task.list", json!({"after":"t999"})).await["tasks"],
        json!([])
    );
    assert_eq!(
        rig.ok(
            None,
            "task.list",
            json!({"state":"accepted","open_only":true})
        )
        .await["tasks"],
        json!([])
    );
    for params in [
        json!({"parent":"t999"}),
        json!({"agent_id":"project/missing"}),
        json!({"creator":"project/missing"}),
    ] {
        assert_eq!(
            rig.call(None, "task.list", params)
                .await
                .unwrap_err()
                .0
                .code,
            "not_found"
        );
    }
    for field in ["creator", "by", "from", "lead", "project_root"] {
        let mut p = json!({"title":"Bad","measure":{"kind":"owner"}});
        p[field] = json!("owner");
        assert_eq!(
            rig.call(None, "task.create", p).await.unwrap_err().0.code,
            "invalid_params"
        );
    }
    rig.ok(
        None,
        "task.set_state",
        json!({"task_id":id,"state":"cancelled","reason":"stopped"}),
    )
    .await;
    assert_eq!(
        rig.ok(None, "task.list", json!({"open_only":true})).await["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        rig.ok(None, "task.get", json!({"task_id":id})).await["task"]["reason"],
        "stopped"
    );
}

// Break caught: UTF-8 limits count characters rather than bytes, or inclusive limits reject the boundary.
#[tokio::test]
async fn every_text_and_vector_bound_is_inclusive_and_byte_based() {
    let disabled = router(MethodServices::new(spawn_registry())).unwrap();
    for (method, field, max, base) in [
        (
            "task.create",
            "title",
            1024,
            json!({"title":"x","measure":{"kind":"owner"}}),
        ),
        (
            "task.set_state",
            "reason",
            16384,
            json!({"task_id":"t6","state":"blocked"}),
        ),
        (
            "task.verdict",
            "note",
            16384,
            json!({"task_id":"t6","verdict":"accepted"}),
        ),
    ] {
        for extra in [false, true] {
            let mut p = base.clone();
            p[field] = json!(format!(
                "{}{}",
                "é".repeat(max / 2),
                if extra { "x" } else { "" }
            ));
            assert_eq!(
                disabled
                    .call(caller(None), method, p)
                    .await
                    .unwrap_err()
                    .0
                    .code,
                if extra {
                    "invalid_params"
                } else {
                    "unavailable"
                }
            );
        }
    }
    for (field, max) in [("path", 4096), ("note", 4096)] {
        for extra in [false, true] {
            let mut file = json!({"path":"p","kind":"brief"});
            file[field] = json!(format!(
                "{}{}",
                "é".repeat(max / 2),
                if extra { "x" } else { "" }
            ));
            assert_eq!(
                disabled
                    .call(
                        caller(None),
                        "task.attach",
                        json!({"task_id":"t6","file":file})
                    )
                    .await
                    .unwrap_err()
                    .0
                    .code,
                if extra {
                    "invalid_params"
                } else {
                    "unavailable"
                }
            );
        }
    }
    for (kind, field, max) in [
        ("mechanical", "command", 16384),
        ("reviewed", "rubric", 4096),
    ] {
        for extra in [false, true] {
            let mut measure = json!({"kind":kind});
            measure[field] = json!(format!(
                "{}{}",
                "é".repeat(max / 2),
                if extra { "x" } else { "" }
            ));
            assert_eq!(
                disabled
                    .call(
                        caller(None),
                        "task.create",
                        json!({"title":"x","measure":measure})
                    )
                    .await
                    .unwrap_err()
                    .0
                    .code,
                if extra {
                    "invalid_params"
                } else {
                    "unavailable"
                }
            );
        }
    }
    for count in [32, 33] {
        assert_eq!(
            disabled
                .call(
                    caller(None),
                    "task.submit",
                    json!({"task_id":"t6","files":vec![json!({"path":"p","kind":"report"});count]})
                )
                .await
                .unwrap_err()
                .0
                .code,
            if count == 32 {
                "unavailable"
            } else {
                "invalid_params"
            }
        );
    }
    for limit in [0, 1, 1000, 1001] {
        assert_eq!(
            disabled
                .call(caller(None), "task.list", json!({"limit":limit}))
                .await
                .unwrap_err()
                .0
                .code,
            if limit == 0 || limit == 1001 {
                "invalid_params"
            } else {
                "unavailable"
            }
        );
    }
}

// Break caught: attachment growth causes silent message truncation, rollback, or a blind enqueue retry.
#[tokio::test]
async fn cumulative_files_over_send_bound_leave_explicit_partial_success() {
    let rig = Rig::new().await;
    for i in 0..32 {
        rig.ok(
            None,
            "task.attach",
            json!({"task_id":"t6","file":{"path":format!("brief-{i}.md"),"kind":"brief"}}),
        )
        .await;
    }
    let result = rig
        .ok(
            None,
            "task.assign",
            json!({"task_id":"t6","agent_id":"project/worker"}),
        )
        .await;
    assert_eq!(result["changed"], true);
    assert_eq!(result["notification"]["status"], "unconfirmed");
    assert_eq!(result["notification"]["error"]["code"], "invalid_params");
    let state = rig.snapshot().await;
    assert_eq!(
        state.task(&"t6".parse().unwrap()).unwrap().state,
        TaskState::Assigned
    );
    assert_eq!(state.task(&"t6".parse().unwrap()).unwrap().files.len(), 33);
    assert_eq!(state.messages().count(), 0);
    assert_eq!(
        rig.ok(
            None,
            "task.assign",
            json!({"task_id":"t6","agent_id":"project/worker"})
        )
        .await["changed"],
        false
    );
    assert_eq!(rig.snapshot().await, state);
}

// Break caught: empty files are treated as an error or a submitted plan is mislabeled as a result.
#[tokio::test]
async fn no_file_notifications_are_actionable_and_plan_is_not_a_result() {
    let rig = Rig::new().await;
    let created = rig
        .ok(
            None,
            "task.create",
            json!({"title":"No files","measure":{"kind":"owner"}}),
        )
        .await;
    let id = created["task_id"].as_str().unwrap();
    let assigned = rig
        .ok(
            None,
            "task.assign",
            json!({"task_id":id,"agent_id":"project/worker"}),
        )
        .await;
    let state = rig.snapshot().await;
    let message = state
        .message(
            &assigned["notification"]["message_id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(message.content.body,"Task t7 assigned: No files\nInspect the task: atm task get t7\nNo brief file is attached; inspect the task before starting.");
    assert!(message.content.attachments.is_empty());
    let result = rig
        .ok(
            Some("%2"),
            "task.submit",
            json!({"task_id":id,"what":"plan"}),
        )
        .await;
    let state = rig.snapshot().await;
    let message = state
        .message(
            &result["notification"]["message_id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(message.content.body,"Task t7 submitted (plan): No files\nSubmitted by project/worker; inspect the task: atm task get t7\nNo submission file is attached.");
}

// Break caught: oversized facts from an internal writer are truncated rather than explicitly refused.
#[tokio::test]
async fn oversized_get_and_single_list_entry_are_refused_without_truncation() {
    let rig = Rig::new().await;
    rig.run(TaskCommand::Create(created(
        "x".repeat(512 * 1024).as_str(),
    )))
    .await;
    assert_eq!(
        rig.call(None, "task.get", json!({"task_id":"t7"}))
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
    let page = rig.ok(None, "task.list", json!({})).await;
    assert_eq!(page["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(page["next_after"], "t6");
    assert_eq!(
        rig.call(None, "task.list", json!({"after":"t6"}))
            .await
            .unwrap_err()
            .0
            .code,
        "refused"
    );
}
