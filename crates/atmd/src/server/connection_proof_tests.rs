// Synthetic identity/observation schedules over unchanged real captured frames.
// Catches duplicated proof reads, inconsistent consumers and cross-frame reuse.

async fn proof_org() -> (tempfile::TempDir, crate::org::OrgHandle, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let org = crate::org::OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
    for command in [
        atm_org::OrgCommand::RegisterProject {
            project_id: "demo".parse().unwrap(),
            root: dir.path().to_str().unwrap().into(),
            move_root: false,
        },
        atm_org::OrgCommand::Adopt {
            agent_id: "demo/worker".parse().unwrap(),
            pane: "%7".into(),
            session_id: Some(SessionId::new("evidence-session")),
        },
    ] {
        org.execute(command, chrono::Utc::now()).await.unwrap();
    }
    (dir, org, cancel)
}

async fn proof_usage(dir: &std::path::Path, org: &crate::org::OrgHandle) -> Vec<serde_json::Value> {
    org.snapshot().await.unwrap(); // FIFO barrier after nonblocking observations.
    let path = dir.join("state");
    tokio::task::spawn_blocking(move || {
        crate::eventlog::read_from(&path, crate::eventlog::Stream::Events, 0)
            .map(Result::unwrap)
            .filter(|event| event.kind == "task.usage_observed")
            .map(|event| event.body)
            .collect()
    })
    .await
    .unwrap()
}

fn proof_registry(
    handler: &mut EvidenceHandler,
    replies: Vec<FakeSnapshot>,
    apply: Result<(), crate::registry::RegistryError>,
) -> (
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use crate::registry::RegistryCommand;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
    let (events, _) = broadcast::channel(16);
    handler.registry = RegistryHandle::new(sender, events);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = count.clone();
    let actor = tokio::spawn(async move {
        let mut replies: std::collections::VecDeque<_> = replies.into();
        let mut held = Vec::new();
        while let Some(command) = receiver.recv().await {
            match command {
                RegistryCommand::ApplyLifecycleEvent { respond_to, .. }
                | RegistryCommand::UpdateFromStatusLine { respond_to, .. } => {
                    let _ = respond_to.send(apply.clone());
                }
                RegistryCommand::GetAllSessions { respond_to } => {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    match replies.pop_front().unwrap_or(FakeSnapshot::Rows(vec![])) {
                        FakeSnapshot::Rows(rows) => {
                            let _ = respond_to.send(rows);
                        }
                        FakeSnapshot::DropReply => drop(respond_to),
                        FakeSnapshot::Hold => held.push(respond_to),
                    }
                }
                _ => panic!("unexpected registry operation"),
            }
        }
    });
    (count, actor)
}

fn proof_pi_usage() -> serde_json::Value {
    let mut data =
        include_str!("../../../atm-pi-adapter/tests/fixtures/pi_session_cost_0.73.1.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["data"].clone())
            .find(|data| data.get("usage").is_some())
            .unwrap();
    data["session_id"] = serde_json::json!("evidence-session");
    data["tmux_pane"] = serde_json::json!("%7");
    data.as_object_mut().unwrap().remove("pid");
    data
}

// A changing scripted registry would make independent consumers disagree. Both
// must use the first post-apply proof, and both real side effects must occur.
#[tokio::test]
async fn frame_proof_is_shared_by_evidence_and_accounting() {
    use atm_core::Harness;
    for (case, harness, expected_usage) in [
        ("claude", Harness::ClaudeCode, 1),
        ("pi-prompt", Harness::Pi, 1),
        ("codex", Harness::Codex, 2),
        ("pi-usage", Harness::Pi, 1),
        ("status", Harness::ClaudeCode, 1),
    ] {
        let (mut handler, evidence) = evidence_handler(harness).await;
        let row = evidence_row(&handler).await;
        let (dir, org, cancel) = proof_org().await;
        handler = handler.with_org(org.clone());
        let (count, actor) = proof_registry(
            &mut handler,
            vec![FakeSnapshot::Rows(vec![row]), FakeSnapshot::Rows(vec![])],
            Ok(()),
        );
        match case {
            "claude" => handler
                .handle_hook_event(captured_prompt(harness))
                .await
                .unwrap(),
            "pi-prompt" => handler
                .handle_pi_event(captured_prompt(harness))
                .await
                .unwrap(),
            "codex" => {
                let rollout = dir.path().join("rollout.jsonl");
                std::fs::write(&rollout,include_str!("../../../atm-codex-adapter/tests/fixtures/codex_rollout_tokens_0.163.0.jsonl")).unwrap();
                let mut data = captured_prompt(harness);
                data["transcript_path"] = serde_json::json!(rollout);
                handler.handle_codex_event(data).await.unwrap();
            }
            "pi-usage" => handler.handle_pi_event(proof_pi_usage()).await.unwrap(),
            _ => {
                let mut data: serde_json::Value = serde_json::from_str(include_str!("../../../atm-claude-adapter/tests/fixtures/claude_status_line_2.1.310.jsonl").lines().next().unwrap()).unwrap();
                data["session_id"] = serde_json::json!("evidence-session");
                data["tmux_pane"] = serde_json::json!("%7");
                handler.handle_status_update(data).await.unwrap();
            }
        }
        let usage = proof_usage(dir.path(), &org).await;
        assert_eq!(usage.len(), expected_usage, "{case}");
        let receipt = evidence.receipt(m123()).unwrap();
        assert_eq!(
            receipt.is_some(),
            matches!(case, "claude" | "pi-prompt" | "codex"),
            "{case}"
        );
        assert!(evidence.snapshot(m123()).unwrap().probably_landed);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1, "{case}");
        drop(handler);
        actor.await.unwrap();
        cancel.cancel();
    }
}

#[tokio::test]
async fn frame_proof_never_survives_the_next_frame_identity_change() {
    let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
    let row = evidence_row(&handler).await;
    let mut replacement = row.clone();
    replacement.id = SessionId::new("replacement");
    let (dir, org, cancel) = proof_org().await;
    handler = handler.with_org(org.clone());
    let (count, actor) = proof_registry(
        &mut handler,
        vec![
            FakeSnapshot::Rows(vec![row]),
            FakeSnapshot::Rows(vec![replacement]),
        ],
        Ok(()),
    );
    handler
        .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
        .await
        .unwrap();
    assert!(evidence.receipt(m123()).unwrap().is_some());
    let id = "m124".parse().unwrap();
    evidence
        .register(id, "[m124] Read: atm inbox --id m124".into())
        .unwrap();
    evidence
        .arm(crate::org::message_evidence::AttemptScope {
            message_id: id,
            attempt_seq: 125,
            recipient: "demo/worker".parse().unwrap(),
            pane: crate::control::reservation::PaneId::parse("%7").unwrap(),
            harness: atm_core::Harness::Pi,
            session_id: Some(SessionId::new("evidence-session")),
            excluded_sessions: Default::default(),
        })
        .unwrap();
    let mut next = captured_prompt(atm_core::Harness::Pi);
    next["payload"]["text"] = serde_json::json!("[m124] Read: atm inbox --id m124");
    handler.handle_pi_event(next).await.unwrap();
    assert!(evidence.receipt(id).unwrap().is_none());
    assert!(evidence.snapshot(id).unwrap().probably_landed);
    assert_eq!(proof_usage(dir.path(), &org).await.len(), 1);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    drop(handler);
    actor.await.unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn frame_proof_skips_reads_without_a_provable_consumer() {
    use atm_core::{ChildAgent, ChildRef, Harness, LifecycleContext, LifecycleEvent};
    for case in ["disabled", "bad-pane", "child", "end", "apply-error"] {
        let (mut handler, evidence) = evidence_handler(Harness::Pi).await;
        let (dir, org, cancel) = proof_org().await;
        if case != "disabled" {
            handler = handler.with_org(org.clone());
        } else {
            handler.message_evidence = None;
        }
        let result = if case == "apply-error" {
            Err(crate::registry::RegistryError::RegistryFull { max: 100 })
        } else {
            Ok(())
        };
        let (count, actor) = proof_registry(&mut handler, vec![], result);
        let context = if case == "child" {
            LifecycleContext {
                child: Some(ChildAgent {
                    reference: ChildRef::Id("child".into()),
                    agent_type: AgentType::GeneralPurpose,
                }),
                ..Default::default()
            }
        } else {
            LifecycleContext::default()
        };
        let event = if case == "end" {
            LifecycleEvent::SessionEnd { reason: None }
        } else {
            LifecycleEvent::PromptSubmit {
                prompt: Some("[m123] Read: atm inbox --id m123".into()),
            }
        };
        let pane = if case == "bad-pane" { "invalid" } else { "%7" };
        let applied = handler
            .apply_message_lifecycle(
                SessionId::new("evidence-session"),
                event,
                Harness::Pi,
                None,
                Some(pane.into()),
                context,
            )
            .await;
        assert_eq!(applied.is_err(), case == "apply-error");
        assert!(evidence.receipt(m123()).unwrap().is_none());
        assert!(proof_usage(dir.path(), &org).await.is_empty());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0, "{case}");
        drop(handler);
        actor.await.unwrap();
        cancel.cancel();
    }
}

#[tokio::test]
async fn frame_proof_one_enabled_consumer_still_observes_the_event() {
    for accounting in [false, true] {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let row = evidence_row(&handler).await;
        let (dir, org, cancel) = proof_org().await;
        if accounting {
            handler = handler.with_org(org.clone());
            handler.message_evidence = None;
        }
        let (count, actor) =
            proof_registry(&mut handler, vec![FakeSnapshot::Rows(vec![row])], Ok(()));
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        assert_eq!(
            proof_usage(dir.path(), &org).await.len(),
            usize::from(accounting)
        );
        assert_eq!(evidence.receipt(m123()).unwrap().is_some(), !accounting);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(handler);
        actor.await.unwrap();
        cancel.cancel();
    }
}

#[test]
fn frame_proof_borrowed_rows_preserve_original_identity_and_uniqueness() {
    let mut domain = SessionDomain::new(
        SessionId::new("parent"),
        AgentType::GeneralPurpose,
        Model::Unknown,
    );
    domain.harness = atm_core::Harness::Pi;
    domain.tmux_pane = Some("%7".into());
    let own = SessionView::from_domain(&domain);
    let mut other = own.clone();
    other.id = SessionId::new("other");
    let mut wrong_harness = own.clone();
    wrong_harness.harness = "codex".into();
    let mut child = own.clone();
    child.parent_session_id = Some(SessionId::new("parent"));
    let mut wrong_pane = own.clone();
    wrong_pane.tmux_pane = Some("%8".into());
    for (rows, expected) in [
        (vec![], false),
        (vec![own.clone()], true),
        (vec![own.clone(), own.clone()], false),
        (vec![own.clone(), other], false),
        (vec![wrong_harness], false),
        (vec![child.clone()], false),
        (vec![own, child], true),
        (vec![wrong_pane], false),
    ] {
        assert_eq!(
            parent_rows_proven(
                &rows,
                &SessionId::new("parent"),
                "%7",
                atm_core::Harness::Pi
            ),
            expected
        );
    }
}

#[tokio::test]
async fn frame_proof_failure_is_shared_and_keeps_the_one_second_cap() {
    for held in [false, true] {
        let (mut handler, evidence) = evidence_handler(atm_core::Harness::Pi).await;
        let (dir, org, cancel) = proof_org().await;
        handler = handler.with_org(org.clone());
        let snapshot = if held {
            FakeSnapshot::Hold
        } else {
            FakeSnapshot::DropReply
        };
        let (count, actor) =
            proof_registry(&mut handler, vec![snapshot, FakeSnapshot::Hold], Ok(()));
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        handler
            .handle_pi_event(captured_prompt(atm_core::Harness::Pi))
            .await
            .unwrap();
        // Paused time only: Tokio rounds to its millisecond timer wheel. One
        // timeout stays in second one; two independent timeouts reach two.
        if held {
            assert_eq!((tokio::time::Instant::now() - start).as_secs(), 1);
        }
        tokio::time::resume();
        assert!(evidence.receipt(m123()).unwrap().is_none());
        assert!(evidence.snapshot(m123()).unwrap().probably_landed);
        assert!(proof_usage(dir.path(), &org).await.is_empty());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(handler);
        actor.await.unwrap();
        cancel.cancel();
    }
}
