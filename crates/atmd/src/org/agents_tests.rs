use super::agents::*;
use crate::server::methods::{router, CallContext, CallerId, MethodServices};
use serde_json::{json, Value};

mod agent_project {
    use super::super::{agent_project::AgentProjects, agents::AgentError, OrgHandle};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use tokio::sync::Barrier;
    use tokio_util::sync::CancellationToken;

    struct Rig {
        dir: tempfile::TempDir,
        org: OrgHandle,
        projects: AgentProjects,
        cancel: CancellationToken,
    }
    impl Rig {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
            let (projects, _) = AgentProjects::spawn(org.clone(), cancel.clone());
            Self {
                dir,
                org,
                projects,
                cancel,
            }
        }
        fn root(&self, name: &str) -> PathBuf {
            let root = self.dir.path().join(name);
            fs::create_dir_all(&root).unwrap();
            root
        }
        fn role(root: &Path, text: &str) {
            fs::create_dir_all(root.join(".atm/roles")).unwrap();
            fs::write(root.join(".atm/roles/lead.md"), text).unwrap();
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    #[tokio::test]
    async fn first_use_creates_once() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        let mut events = rig.org.committed.subscribe();
        let barrier = Arc::new(Barrier::new(3));
        let mut jobs = Vec::new();
        for _ in 0..2 {
            let projects = rig.projects.clone();
            let root = root.clone();
            let barrier = barrier.clone();
            jobs.push(tokio::spawn(async move {
                barrier.wait().await;
                projects.identify(root, "lead".into()).await.unwrap()
            }));
        }
        barrier.wait().await;
        for job in jobs {
            assert_eq!(job.await.unwrap().to_string(), "project/lead");
        }
        assert_eq!(
            fs::read_to_string(root.join(".atm/project.toml")).unwrap(),
            "id = \"project\"\n"
        );
        assert_eq!(rig.org.status().unwrap(), 1);
        let event = events.recv().await.unwrap();
        assert_eq!(event.kind, "project.registered");
        assert_eq!(event.seq, 1);
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        let state = rig.org.snapshot().await.unwrap();
        assert_eq!(state.projects().count(), 1);
        assert_eq!(
            state.projects().next().unwrap().root,
            root.to_str().unwrap()
        );
        assert_eq!(fs::read_dir(root.join(".atm")).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn same_slug_different_roots() {
        let rig = Rig::new().await;
        let a = rig.root("a/project");
        let b = rig.root("b/project");
        let (a, b) = tokio::join!(
            rig.projects.identify(a, "lead".into()),
            rig.projects.identify(b, "lead".into())
        );
        let mut ids = vec![a.unwrap().to_string(), b.unwrap().to_string()];
        ids.sort();
        assert_eq!(ids, ["project-2/lead", "project/lead"]);
        assert_eq!(rig.org.status().unwrap(), 2);
    }

    #[tokio::test]
    async fn linked_root_uses_main_role() {
        let rig = Rig::new().await;
        let main = rig.root("main");
        let linked = rig.root("linked");
        let meta = main.join(".git/worktrees/linked");
        fs::create_dir_all(&meta).unwrap();
        fs::write(linked.join(".git"), format!("gitdir: {}\n", meta.display())).unwrap();
        fs::write(meta.join("commondir"), "../..\n").unwrap();
        fs::write(
            meta.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();
        fs::create_dir(main.join("work")).unwrap();
        Rig::role(
            &main,
            "+++\nharness='pi'\ncwd='work'\ntrust_dirs=['work']\n+++\nmain instructions",
        );
        Rig::role(&linked, "+++\nharness='codex'\n+++\nwrong");
        let prepared = rig
            .projects
            .prepare(linked.clone(), "lead".into())
            .await
            .unwrap();
        assert_eq!(prepared.agent_id.to_string(), "main/lead");
        assert_eq!(prepared.root, main);
        assert_eq!(prepared.role.instructions, "main instructions");
        assert_eq!(prepared.role.config.harness, atm_core::Harness::Pi);
        assert_eq!(prepared.cwd, main.join("work"));
        assert_eq!(prepared.trust_dirs, [main.join("work")]);
        assert!(!linked.join(".atm/project.toml").exists());
    }

    #[tokio::test]
    async fn unsupported_gitdir_refused() {
        let rig = Rig::new().await;
        for pointer in [
            "garbage",
            "gitdir: /not/a/main/.git/modules/module",
            "gitdir: missing",
        ] {
            let root = rig.root("bad");
            fs::write(root.join(".git"), pointer).unwrap();
            assert!(matches!(
                rig.projects.identify(root.clone(), "lead".into()).await,
                Err(AgentError::Refused(_))
            ));
            assert!(!root.join(".atm").exists());
        }
        assert_eq!(rig.org.status().unwrap(), 0);
    }

    async fn copied(rig: &Rig) -> (PathBuf, PathBuf) {
        let old = rig.root("old");
        rig.projects
            .identify(old.clone(), "lead".into())
            .await
            .unwrap();
        let new = rig.root("new");
        fs::create_dir(new.join(".atm")).unwrap();
        fs::copy(old.join(".atm/project.toml"), new.join(".atm/project.toml")).unwrap();
        (old, new)
    }

    #[tokio::test]
    async fn copied_id_refused_old_root_still_exists() {
        let rig = Rig::new().await;
        let (old, new) = copied(&rig).await;
        assert!(matches!(
            rig.projects.identify(new, "lead".into()).await,
            Err(AgentError::Conflict(_))
        ));
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .projects()
                .next()
                .unwrap()
                .root,
            old.to_str().unwrap()
        );
        assert_eq!(rig.org.status().unwrap(), 1);
    }

    #[tokio::test]
    async fn missing_old_root_allows_move() {
        let rig = Rig::new().await;
        let (old, new) = copied(&rig).await;
        fs::remove_dir_all(old).unwrap();
        assert_eq!(
            rig.projects
                .identify(new.clone(), "lead".into())
                .await
                .unwrap()
                .to_string(),
            "old/lead"
        );
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .projects()
                .next()
                .unwrap()
                .root,
            new.to_str().unwrap()
        );
        assert_eq!(rig.org.status().unwrap(), 2);
    }

    #[tokio::test]
    async fn invalid_or_unreadable_old_root_not_move_permission() {
        let rig = Rig::new().await;
        let (old, new) = copied(&rig).await;
        let file = old.join(".atm/project.toml");
        fs::write(&file, "invalid TOML").unwrap();
        assert!(matches!(
            rig.projects.identify(new.clone(), "lead".into()).await,
            Err(AgentError::Refused(_))
        ));
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap(); // EISDIR, deterministic even under root.
        assert!(rig.projects.identify(new, "lead".into()).await.is_err());
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .projects()
                .next()
                .unwrap()
                .root,
            old.to_str().unwrap()
        );
        assert_eq!(rig.org.status().unwrap(), 1);
    }

    #[tokio::test]
    async fn dangling_old_root_is_not_move_permission() {
        let rig = Rig::new().await;
        let (old, new) = copied(&rig).await;
        fs::remove_dir_all(&old).unwrap();
        std::os::unix::fs::symlink(rig.dir.path().join("missing"), &old).unwrap();
        assert!(matches!(
            rig.projects.identify(new, "lead".into()).await,
            Err(AgentError::Refused(_))
        ));
        assert_eq!(rig.org.status().unwrap(), 1);
    }

    #[tokio::test]
    async fn malformed_recorded_root_is_not_move_permission() {
        let rig = Rig::new().await;
        let new = rig.root("new");
        fs::create_dir(new.join(".atm")).unwrap();
        fs::write(new.join(".atm/project.toml"), "id='old'\n").unwrap();
        rig.org
            .execute(
                atm_org::OrgCommand::RegisterProject {
                    project_id: "old".parse().unwrap(),
                    root: "relative-missing".into(),
                    move_root: false,
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        assert!(matches!(
            rig.projects.identify(new, "lead".into()).await,
            Err(AgentError::Refused(_))
        ));
        assert_eq!(rig.org.status().unwrap(), 1);
    }

    #[tokio::test]
    async fn list_does_not_create() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        assert_eq!(rig.projects.filter(root.clone()).await.unwrap(), None);
        assert!(!root.join(".atm").exists());
        fs::create_dir(root.join(".atm")).unwrap();
        fs::write(root.join(".atm/project.toml"), "id='existing'\n").unwrap();
        assert_eq!(
            rig.projects
                .filter(root.clone())
                .await
                .unwrap()
                .unwrap()
                .to_string(),
            "existing"
        );
        fs::write(root.join(".atm/project.toml"), "invalid").unwrap();
        assert!(matches!(
            rig.projects.filter(root).await,
            Err(AgentError::Refused(_))
        ));
        assert_eq!(rig.org.status().unwrap(), 0);
    }

    #[tokio::test]
    async fn missing_role_and_nonabsolute_root_fail_before_registration() {
        let rig = Rig::new().await;
        assert!(matches!(
            rig.projects
                .identify("relative".into(), "lead".into())
                .await,
            Err(AgentError::InvalidParams(_))
        ));
        let root = rig.root("project");
        assert!(matches!(
            rig.projects.prepare(root.clone(), "lead".into()).await,
            Err(AgentError::NotFound(_))
        ));
        assert!(!root.join(".atm").exists());
        assert_eq!(rig.org.status().unwrap(), 0);
    }

    #[tokio::test]
    async fn git_environment_cannot_redirect_project_io() {
        // Run the real linked-root test in a subprocess to avoid changing this
        // test process's environment concurrently. No Git subprocess is needed
        // by either the implementation or its synthetic checkout fixtures.
        let decoy = tempfile::tempdir().unwrap();
        let git = decoy.path().join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/decoy\n").unwrap();
        fs::write(git.join("config"), "[core]\nrepositoryformatversion=0\n").unwrap();
        fs::write(git.join("index"), "decoy sentinel").unwrap();
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "org::agents_tests::agent_project::linked_root_uses_main_role",
                    "--nocapture",
                ])
                .env("GIT_DIR", &git)
                .env("GIT_INDEX_FILE", git.join("index"))
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert_eq!(
            fs::read_to_string(git.join("index")).unwrap(),
            "decoy sentinel"
        );
        assert_eq!(
            fs::read_to_string(git.join("HEAD")).unwrap(),
            "ref: refs/heads/decoy\n"
        );
        assert_eq!(
            fs::read_to_string(git.join("config")).unwrap(),
            "[core]\nrepositoryformatversion=0\n"
        );
        assert_eq!(fs::read_dir(git.join("objects")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(git.join("refs/heads")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn role_name_traversal_refused() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        for name in ["../secret", "/absolute", "a/b", "owner"] {
            assert!(matches!(
                rig.projects.prepare(root.clone(), name.into()).await,
                Err(AgentError::InvalidParams(_))
            ));
            assert!(matches!(
                rig.projects.identify(root.clone(), name.into()).await,
                Err(AgentError::InvalidParams(_))
            ));
        }
        assert!(!root.join(".atm").exists());
    }

    #[tokio::test]
    async fn bad_respawn_role_precedes_kill() {
        let rig = Rig::new().await;
        let root = rig.root("project");
        // The same preparation seam is required before a respawn can acquire
        // its pane reservation. No lifecycle intent is recorded on failure.
        for role in [
            "+++\nharness='pi'\ntypo='secret'\n+++\nprivate",
            "+++\nharness='pi'\ncwd='missing'\n+++",
            "+++\nharness='pi'\ntrust_dirs=['missing']\n+++",
        ] {
            Rig::role(&root, role);
            let error = rig
                .projects
                .prepare(root.clone(), "lead".into())
                .await
                .unwrap_err();
            assert!(matches!(error, AgentError::Refused(_)));
            assert!(!error.to_string().contains("private"));
            assert!(!error.to_string().contains("secret"));
            assert_eq!(rig.org.status().unwrap(), 0);
            assert!(!root.join(".atm/project.toml").exists());
        }
    }
}

#[tokio::test]
async fn agent_contract_disabled_has_no_effects() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let org = super::OrgHandle::open(dir.path().into(), cancel.clone()).await;
    let registry = crate::registry::spawn_registry();
    let mock = atm_tmux::MockTmuxClient::new();
    let (control, control_task) = crate::control::service::spawn_control_with_org(
        Box::new(mock.clone()),
        registry.clone(),
        org.clone(),
        cancel.clone(),
    );
    let before = org.status().unwrap();
    let (agents, task) = spawn_agents(org.clone(), registry, control, cancel.clone());
    let mut services = MethodServices::new(crate::registry::spawn_registry());
    services.agents = Some(agents.clone());
    let router = router(services).unwrap();
    for (method, params) in requests() {
        let error = router.call(caller(), method, params).await.unwrap_err();
        assert_eq!(error.0.code, "unavailable");
        assert_eq!(
            error.0.message,
            format!("agent operation '{method}' is not implemented")
        );
    }
    assert!(matches!(
        agents.reconciled().await,
        Err(AgentError::NotImplemented { .. })
    ));
    assert!(
        matches!(AgentHandle::disabled("off").reconciled().await, Err(AgentError::Unavailable(reason)) if reason == "off")
    );
    task.await.unwrap();
    assert!(mock.calls().is_empty());
    assert_eq!(org.status().unwrap(), before);
    assert!(org.snapshot().await.unwrap().agents().next().is_none());
    cancel.cancel();
    control_task.await.unwrap();
}

#[test]
fn agent_contract_schemas_keep_defaults_and_optional_session() {
    let clear: ClearParams = serde_json::from_value(json!({"agent":"project/lead"})).unwrap();
    assert_eq!(clear.timeout_ms, 30_000);
    assert!(
        !serde_json::from_value::<DownParams>(json!({"agent":"project/lead"}))
            .unwrap()
            .kill
    );
    assert!(serde_json::from_value::<ListParams>(json!({}))
        .unwrap()
        .project_root
        .is_none());
    assert!(serde_json::from_value::<ClearParams>(
        json!({"agent":"project/lead","timeout_ms":null})
    )
    .is_err());
    let result = ClearResult {
        agent_id: clear.agent,
        session_id: None,
    };
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({"agent_id":"project/lead","session_id":null})
    );
    let roundtrip: ClearResult =
        serde_json::from_value(json!({"agent_id":"project/lead","session_id":null,"future":true}))
            .unwrap();
    assert_eq!(roundtrip.agent_id, result.agent_id);
    assert!(roundtrip.session_id.is_none());
    for (method, params) in requests() {
        let value = match method {
            "agent.up" => {
                serde_json::to_value(serde_json::from_value::<UpParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.adopt" => {
                serde_json::to_value(serde_json::from_value::<AdoptParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.clear" => {
                serde_json::to_value(serde_json::from_value::<ClearParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.respawn" => serde_json::to_value(
                serde_json::from_value::<RespawnParams>(params.clone()).unwrap(),
            )
            .unwrap(),
            "agent.down" => {
                serde_json::to_value(serde_json::from_value::<DownParams>(params.clone()).unwrap())
                    .unwrap()
            }
            "agent.list" => {
                serde_json::to_value(serde_json::from_value::<ListParams>(params.clone()).unwrap())
                    .unwrap()
            }
            _ => unreachable!(),
        };
        for (key, expected) in params.as_object().unwrap() {
            assert_eq!(value.get(key), Some(expected), "{method}/{key}");
        }
    }
}

#[test]
fn agent_contract_error_codes_preserve_actionable_messages() {
    use super::OrgServiceError;
    use crate::control::{
        reservation::{PaneId, ReservationError},
        ControlError,
    };
    use crate::server::methods::MethodError;
    let cases = [
        (
            AgentError::InvalidParams("bad root".into()),
            "invalid_params",
        ),
        (
            AgentError::NotFound("agent project/lead was not found".into()),
            "not_found",
        ),
        (AgentError::Conflict("busy".into()), "conflict"),
        (AgentError::Refused("unsafe editor".into()), "refused"),
        (AgentError::Timeout, "timeout"),
        (AgentError::Internal("storage".into()), "internal"),
        (ReservationError::Unavailable.into(), "unavailable"),
        (OrgServiceError::Timeout.into(), "timeout"),
        (
            OrgServiceError::Unavailable("disk".into()).into(),
            "unavailable",
        ),
        (
            OrgServiceError::Refused(atm_org::Refusal::Invariant(
                atm_org::ApplyError::UnknownAgent("project/lead".parse().unwrap()),
            ))
            .into(),
            "not_found",
        ),
    ];
    for (error, code) in cases {
        let message = error.to_string();
        let mapped = MethodError::from(error);
        assert_eq!(mapped.0.code, code);
        assert_eq!(mapped.0.message, message);
    }
    let error = MethodError::from(AgentError::from(ReservationError::ScreenChanged {
        pane: PaneId::parse("%7").unwrap(),
    }));
    assert_eq!(error.0.code, "refused");
    assert_eq!(
        error.0.message,
        "Refusing input to %7: screen changed or is no longer safe; nothing was sent by this action"
    );
    let error = MethodError::from(AgentError::from(ReservationError::Control(
        ControlError::AgentClearing {
            pane: "%7".into(),
            agent: "project/lead".parse().unwrap(),
        },
    )));
    assert_eq!(error.0.code, "refused");
    assert_eq!(
        error.0.message,
        "Refusing to send to %7: agent project/lead is clearing; try again in a moment"
    );
}

fn caller() -> CallContext {
    CallContext {
        client_id: CallerId("contract".into()),
        from_pane: None,
    }
}

fn requests() -> Vec<(&'static str, Value)> {
    vec![
        ("agent.up", json!({"project_root":"/project","name":"lead"})),
        (
            "agent.adopt",
            json!({"project_root":"/project","name":"lead","pane":"%7"}),
        ),
        ("agent.clear", json!({"agent":"project/lead"})),
        ("agent.respawn", json!({"agent":"project/lead"})),
        ("agent.down", json!({"agent":"project/lead"})),
        ("agent.list", json!({})),
    ]
}

// Missing registration must not masquerade as an old daemon; unavailable is the
// checkpoint contract, not unknown_method or a fabricated successful result.
#[tokio::test]
async fn agent_contract_advertises_six_disabled_methods() {
    let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    let info = router
        .call(caller(), "daemon.info", Value::Null)
        .await
        .unwrap();
    for (method, params) in requests() {
        assert!(router.names().iter().any(|name| name == method), "{method}");
        assert!(info["methods"].as_array().unwrap().contains(&json!(method)));
        let error = router.call(caller(), method, params).await.unwrap_err();
        assert_eq!(error.0.code, "unavailable", "{method}");
        assert_eq!(
            error.0.message,
            "agent service unavailable: named-agent operations are not configured"
        );
    }
}

#[tokio::test]
async fn agent_contract_rejects_unknown_and_nonobject_parameters() {
    let router = router(MethodServices::new(crate::registry::spawn_registry())).unwrap();
    for (method, mut params) in requests() {
        params
            .as_object_mut()
            .unwrap()
            .insert("typo".into(), json!(true));
        for bad in [params, json!([]), json!(true), json!("bad")] {
            let error = router.call(caller(), method, bad).await.unwrap_err();
            assert_eq!(error.0.code, "invalid_params", "{method}");
        }
    }
}
