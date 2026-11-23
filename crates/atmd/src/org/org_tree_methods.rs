//! One bounded fold/registry/file join, shared by every organisation-tree caller.
use super::{
    agents::AgentError, department_directory::DepartmentFiles, org_tree::project,
    reporting_line::ReportingFiles, OrgHandle,
};
use crate::{
    registry::RegistryHandle,
    server::methods::{parse_params, MethodError, MethodRouter},
};
use atm_protocol::error_code;
use serde::Deserialize;
use std::time::Duration;

pub(crate) const REPLY_BYTES: usize = 512 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeParams {}

pub(crate) fn register_org_tree_method(
    router: &mut MethodRouter,
    org: OrgHandle,
    registry: RegistryHandle,
    reporting: ReportingFiles,
    departments: DepartmentFiles,
) -> Result<(), MethodError> {
    router.register("org.tree", Duration::from_secs(12), move |_, params| {
        let (org, registry, reporting, departments) = (
            org.clone(),
            registry.clone(),
            reporting.clone(),
            departments.clone(),
        );
        async move {
            parse_params::<TreeParams>(params)?;
            let snapshot = async {
                org.snapshot_at()
                    .await
                    .map_err(AgentError::from)
                    .map_err(MethodError::from)
            };
            let live = async {
                if !registry.is_connected() {
                    return Err(MethodError::new(
                        error_code::UNAVAILABLE,
                        "session registry unavailable",
                    ));
                }
                let rows =
                    tokio::time::timeout(Duration::from_secs(5), registry.get_all_sessions())
                        .await
                        .map_err(|_| {
                            MethodError::new(
                                error_code::TIMEOUT,
                                "reading session registry timed out",
                            )
                        })?;
                if !registry.is_connected() {
                    return Err(MethodError::new(
                        error_code::UNAVAILABLE,
                        "session registry unavailable",
                    ));
                }
                Ok(rows)
            };
            let base = async {
                let ((state, seq), rows) = tokio::try_join!(snapshot, live)?;
                let edges = reporting.edges(state.clone()).await?;
                Ok::<_, MethodError>((state, seq, rows, edges))
            };
            // Do not spend a third serial file budget, or share the authority lane.
            let ((state, seq, rows, edges), directory) =
                tokio::try_join!(base, async { Ok(departments.read().await) })?;
            let value = serde_json::to_value(project(
                &state,
                seq,
                &rows,
                &edges,
                &directory,
                chrono::Utc::now(),
            ))
            .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))?;
            if serde_json::to_vec(&value)
                .map_err(|error| MethodError::new(error_code::INTERNAL, error.to_string()))?
                .len()
                > REPLY_BYTES
            {
                return Err(MethodError::new(
                    error_code::REFUSED,
                    "org.tree result exceeds 512 KiB",
                ));
            }
            Ok(value)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        org::{agent_context::detached_read, Request},
        registry::RegistryCommand,
        server::methods::{router_with_reporting, CallContext, CallerId, MethodServices},
    };
    use atm_org::{OrgCommand, OrgEvent, OrgState};
    use serde_json::json;
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::Poll,
    };
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio_util::sync::CancellationToken;

    struct Rig {
        _dir: tempfile::TempDir,
        org: OrgHandle,
        registry: RegistryHandle,
        snapshots: mpsc::Receiver<()>,
        rows: mpsc::Receiver<()>,
        snapshot_count: Arc<AtomicUsize>,
        row_count: Arc<AtomicUsize>,
        files: ReportingFiles,
        cancel: CancellationToken,
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }
    impl Rig {
        fn new(extra_tasks: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join(".atm/roles")).unwrap();
            std::fs::write(
                dir.path().join(".atm/roles/worker.md"),
                "+++\nharness='pi'\n+++\nSECRET role\n",
            )
            .unwrap();
            let now = "2026-01-15T12:00:00Z".parse().unwrap();
            let mut state = OrgState::default();
            for (seq, event) in [
                (
                    1,
                    OrgEvent::ProjectRegistered {
                        project_id: "alpha".parse().unwrap(),
                        root: dir.path().to_str().unwrap().into(),
                    },
                ),
                (
                    2,
                    OrgEvent::AgentAdopted {
                        agent_id: "alpha/worker".parse().unwrap(),
                        pane: "%1".into(),
                        session_id: None,
                    },
                ),
            ] {
                state.apply(seq, now, &event).unwrap();
            }
            for n in 0..extra_tasks {
                let task=serde_json::from_value(json!({"title":"🙂".repeat(128),"kind":"work","creator":"owner","parent":null,"judges":null,"measure":{"kind":"owner"},"budget":null,"files":[],"rests_on":null})).unwrap();
                state
                    .apply(n as u64 + 3, now, &OrgEvent::TaskCreated(task))
                    .unwrap();
            }
            let cursor = extra_tasks as u64 + 2;
            let cancel = CancellationToken::new();
            let (real_requests, receive) = mpsc::channel(64);
            let (health, status) = watch::channel(Ok(cursor));
            let (committed, _) = broadcast::channel(1024);
            tokio::spawn(crate::org::actor::run(
                state,
                receive,
                health,
                committed.clone(),
                cancel.clone(),
                {
                    let mut seq = cursor;
                    move |_| {
                        seq += 1;
                        std::future::ready(Ok(seq))
                    }
                },
            ));
            let (requests, mut receive) = mpsc::channel(64);
            let (snapshot_tx, snapshots) = mpsc::channel(64);
            let snapshot_count = Arc::new(AtomicUsize::new(0));
            let counts = snapshot_count.clone();
            tokio::spawn(async move {
                while let Some(request) = receive.recv().await {
                    match request {
                        Request::SnapshotAt(reply) => {
                            counts.fetch_add(1, Ordering::SeqCst);
                            let (tx, rx) = tokio::sync::oneshot::channel();
                            real_requests.send(Request::SnapshotAt(tx)).await.unwrap();
                            let result = rx.await.unwrap();
                            let _ = reply.send(result);
                            let _ = snapshot_tx.send(()).await;
                        }
                        request => {
                            if real_requests.send(request).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            let org = OrgHandle {
                requests,
                health: status,
                committed,
                reads: None,
            };
            let (tx, mut rx) = mpsc::channel(64);
            let (events, _) = broadcast::channel(16);
            let (row_tx, rows) = mpsc::channel(64);
            let row_count = Arc::new(AtomicUsize::new(0));
            let counts = row_count.clone();
            tokio::spawn(async move {
                while let Some(request) = rx.recv().await {
                    match request {
                        RegistryCommand::GetAllSessions { respond_to } => {
                            counts.fetch_add(1, Ordering::SeqCst);
                            let _ = respond_to.send(vec![]);
                            let _ = row_tx.send(()).await;
                        }
                        _ => panic!("unexpected registry operation"),
                    }
                }
            });
            Self {
                _dir: dir,
                org,
                registry: RegistryHandle::new(tx, events),
                snapshots,
                rows,
                snapshot_count,
                row_count,
                files: ReportingFiles::default(),
                cancel,
            }
        }
        fn router(&self) -> MethodRouter {
            let mut services = MethodServices::new(self.registry.clone());
            services.org = self.org.clone();
            router_with_reporting(services, self.files.clone()).unwrap()
        }
        async fn sources(&mut self) {
            self.snapshots.recv().await.unwrap();
            self.rows.recv().await.unwrap();
        }
    }
    fn caller() -> CallContext {
        CallContext {
            client_id: CallerId("tree-read".into()),
            from_pane: None,
        }
    }

    // Break caught: a role wait causes a second fold/registry read or pairs old
    // data with the newer cursor; the role lane must not hold actor/service locks.
    #[tokio::test]
    async fn org_tree_keeps_one_snapshot_cursor_while_roles_wait() {
        let mut rig = Rig::new(0);
        let router = rig.router();
        let held = rig.files.slot.clone().acquire_owned().await.unwrap();
        let mut read = Box::pin(router.call(caller(), "org.tree", json!({})));
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        rig.sources().await;
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let new_root = rig._dir.path().join("moved");
        std::fs::create_dir_all(new_root.join(".atm/roles")).unwrap();
        std::fs::write(
            new_root.join(".atm/roles/worker.md"),
            "+++\nharness=17\n+++\nSECRET\n",
        )
        .unwrap();
        rig.org
            .execute(
                OrgCommand::RegisterProject {
                    project_id: "alpha".parse().unwrap(),
                    root: new_root.to_str().unwrap().into(),
                    move_root: true,
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        assert_eq!(
            rig.org
                .snapshot()
                .await
                .unwrap()
                .project(&"alpha".parse().unwrap())
                .unwrap()
                .root,
            new_root.to_str().unwrap()
        );
        drop(held);
        let result = read.await.unwrap();
        assert_eq!(result["seq"], 2);
        assert_eq!(result["projects"][0]["root"], json!(rig._dir.path()));
        assert_eq!(result["projects"][0]["seats"][0]["manager"], "owner");
        assert_eq!(rig.snapshot_count.load(Ordering::SeqCst), 1);
        assert_eq!(rig.row_count.load(Ordering::SeqCst), 1);
        let next = router.call(caller(), "org.tree", json!({})).await.unwrap();
        assert_eq!(next["seq"], 3);
        assert_eq!(
            next["projects"][0]["seats"][0]["line_error"],
            "reporting line at alpha/worker: invalid role configuration"
        );
        assert_eq!(rig.snapshot_count.load(Ordering::SeqCst), 2);
        assert_eq!(rig.row_count.load(Ordering::SeqCst), 2);
    }

    // Break caught: a busy role lane refuses the whole read or invents broken
    // lines; cancelling a waiter must not release somebody else's retained slot.
    #[tokio::test]
    async fn org_tree_reporting_timeout_is_a_complete_fallback() {
        let mut rig = Rig::new(1);
        let router = rig.router();
        let held = rig.files.slot.clone().acquire_owned().await.unwrap();
        let mut read = Box::pin(router.call(caller(), "org.tree", json!({})));
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        rig.sources().await;
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        let result = read.await.unwrap();
        assert_eq!(result["reporting"]["state"], "unavailable");
        assert_eq!(result["seq"], 3);
        assert_eq!(result["unplaced_tasks"].as_array().unwrap().len(), 1);
        assert!(result["projects"][0]["seats"][0]["manager"].is_null());
        assert!(result["projects"][0]["seats"][0]["line_error"].is_null());
        assert_eq!(result["needs_owner"]["items_total"], 0);
        assert_eq!(rig.files.slot.available_permits(), 0);
        tokio::time::resume();
        drop(held);
        rig.files.slot.close();
        assert_eq!(
            router
                .call(caller(), "org.tree", json!({}))
                .await
                .unwrap_err()
                .0
                .code,
            "unavailable"
        );
    }

    // Break caught: an unresponsive registry is read without a bound, or a
    // registry that closes during the read is presented as an empty success.
    #[tokio::test]
    async fn org_tree_registry_deadline_and_late_channel_loss() {
        for timed_out in [true, false] {
            let rig = Rig::new(0);
            let (tx, mut rx) = mpsc::channel(1);
            let (events, _) = broadcast::channel(1);
            let mut services = MethodServices::new(RegistryHandle::new(tx, events));
            services.org = rig.org.clone();
            let router = router_with_reporting(services, rig.files.clone()).unwrap();
            let mut read = Box::pin(router.call(caller(), "org.tree", json!({})));
            std::future::poll_fn(|cx| {
                assert!(read.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let RegistryCommand::GetAllSessions { respond_to } = rx.recv().await.unwrap() else {
                panic!("wrong registry read")
            };
            if timed_out {
                // No sockets or filesystem worker: only the retained in-memory
                // registry response and its actual five-second read budget.
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(5)).await;
                assert_eq!(read.await.unwrap_err().0.code, "timeout");
                assert!(respond_to.send(vec![]).is_err());
                tokio::time::resume();
            } else {
                drop(rx);
                respond_to.send(vec![]).unwrap();
                assert_eq!(read.await.unwrap_err().0.code, "unavailable");
            }
        }
    }

    // Break caught: a dropped caller keeps an actor/registry wait alive, or
    // service shutdown is mistaken for a valid empty fold.
    #[tokio::test]
    async fn org_tree_cancellation_and_actor_shutdown_are_normal() {
        let mut rig = Rig::new(0);
        let router = rig.router();
        let held = rig.files.slot.clone().acquire_owned().await.unwrap();
        let mut read = Box::pin(router.call(caller(), "org.tree", json!({})));
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        rig.sources().await;
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(read);
        assert_eq!(rig.files.slot.available_permits(), 0);
        drop(held);
        let mut health = rig.org.health.clone();
        rig.cancel.cancel();
        health.wait_for(|status| status.is_err()).await.unwrap();
        assert_eq!(
            router
                .call(caller(), "org.tree", json!({}))
                .await
                .unwrap_err()
                .0
                .code,
            "unavailable"
        );
    }

    // Break caught: new fields alter the old fold or lose overlap, live counts, or diagnostics.
    #[tokio::test]
    async fn org_tree_department_fields_golden() {
        let rig = Rig::new(0);
        let dir = rig._dir.path().join("departments");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("apps.toml"), "title='Apps'\ngoal='Ship apps'\nhead='alpha/worker'\nprojects=['alpha']\nroster=[{seats='alpha/worker',max=1}]\n[hire]\nmode='enforce'\n[spend]\nmodels=['PRIVATE-SPEND']\n").unwrap();
        std::fs::write(
            dir.join("tools.toml"),
            "title='Tools'\nprojects=['alpha','missing']\n[[roster]]\nseats='alpha/build-*'\n",
        )
        .unwrap();
        std::fs::write(dir.join("bad.toml"), "title='Bad'\nprojects=[]\n").unwrap();
        let mut services = MethodServices::new(rig.registry.clone());
        services.org = rig.org.clone();
        services.departments_dir = Some(dir.clone());
        let router = router_with_reporting(services, rig.files.clone()).unwrap();
        let tree = router.call(caller(), "org.tree", json!({})).await.unwrap();
        assert_eq!(tree["seq"], 2);
        assert_eq!(tree["projects"].as_array().unwrap().len(), 1);
        assert_eq!(tree["projects"][0]["id"], "alpha");
        assert_eq!(
            tree["departments"],
            json!([
                {"id":"apps","title":"Apps","goal":"Ship apps","head":"alpha/worker","projects":["alpha"],
                 "roster":[{"seats":"alpha/worker","max":1,"live":1}],"hire_mode":"enforce","problems":[]},
                {"id":"tools","title":"Tools","goal":null,"head":null,"projects":["alpha","missing"],
                 "roster":[{"seats":"alpha/build-*","max":null,"live":0}],"hire_mode":"report",
                 "problems":["tools.toml:projects: project missing is not registered"],
                 "would_wait":[{"seat":"alpha/worker","reason":"not on roster"}]}
            ])
        );
        assert_eq!(
            tree["department_problems"],
            json!(["bad.toml:projects: projects must not be empty"])
        );
        assert!(!tree.to_string().contains("PRIVATE-SPEND"));
        std::fs::remove_dir_all(dir).unwrap();
        let next = router.call(caller(), "org.tree", json!({})).await.unwrap();
        assert_eq!(next["seq"], 2);
        assert!(next.get("departments").is_none() && next.get("department_problems").is_none());
    }

    // Break caught: overlapping departments are merged, the cap includes the target
    // twice, or down seats are shown in a report. Rows are ordered by seat id.
    #[tokio::test]
    async fn org_tree_would_wait_uses_each_report_roster() {
        let rig = Rig::new(0);
        for (name, pane) in [
            ("build-3", "%3"),
            ("build-2", "%2"),
            ("build-1", "%4"),
            ("build-4", "%5"),
        ] {
            rig.org
                .execute(
                    OrgCommand::Adopt {
                        agent_id: format!("alpha/{name}").parse().unwrap(),
                        pane: pane.into(),
                        session_id: None,
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
        }
        rig.org
            .execute(
                OrgCommand::Down {
                    agent_id: "alpha/build-4".parse().unwrap(),
                    reason: "fixture".into(),
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        let dir = rig._dir.path().join("departments");
        std::fs::create_dir(&dir).unwrap();
        for (name, roster) in [
            (
                "crowded",
                "[{seats='alpha/worker', max=1}, {seats='alpha/build-*', max=1}]",
            ),
            ("roomy", "['alpha/worker', {seats='alpha/build-*', max=3}]"),
        ] {
            std::fs::write(
                dir.join(format!("{name}.toml")),
                format!("title='{name}'\nprojects=['alpha']\nroster={roster}\n"),
            )
            .unwrap();
        }
        let mut services = MethodServices::new(rig.registry.clone());
        services.org = rig.org.clone();
        services.departments_dir = Some(dir);
        let router = router_with_reporting(services, rig.files.clone()).unwrap();
        let tree = router.call(caller(), "org.tree", json!({})).await.unwrap();
        assert_eq!(tree["departments"][0]["id"], "crowded");
        assert_eq!(
            tree["departments"][0]["would_wait"],
            json!([
                {"seat":"alpha/build-1", "reason":"roster full: 2 of 1 for alpha/build-*"},
                {"seat":"alpha/build-2", "reason":"roster full: 2 of 1 for alpha/build-*"},
                {"seat":"alpha/build-3", "reason":"roster full: 2 of 1 for alpha/build-*"}
            ])
        );
        assert_eq!(tree["departments"][1]["id"], "roomy");
        assert_eq!(tree["departments"][1]["would_wait"], json!([]));
    }

    // Break caught: using a stale up snapshot, deriving the model from manager
    // resolution, or losing the distinction between no role and its default model.
    #[tokio::test]
    async fn org_tree_would_wait_uses_current_roles() {
        let rig = Rig::new(0);
        let dir = rig._dir.path().join("departments");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("tools.toml"),
            "title='Tools'\nprojects=['alpha']\n\
             roster=['alpha/allowed','alpha/broken-line','alpha/default','alpha/failed','alpha/missing','alpha/worker']\n\
             [spend]\nmodels=['test-lite*']\n",
        )
        .unwrap();
        for (name, model) in [
            ("allowed", "test-pro:max"),
            ("both", "test-lite:max"),
            ("broken-line", "test-lite:max"),
            ("default", "test-lite:max"),
            ("down", "test-pro:max"),
            ("failed", "test-pro:max"),
            ("missing", "test-pro:max"),
        ] {
            let role =
                atm_org::parse_role(name, &format!("+++\nharness='pi'\nmodel='{model}'\n+++\n"))
                    .unwrap();
            rig.org
                .execute(
                    OrgCommand::Up {
                        agent_id: format!("alpha/{name}").parse().unwrap(),
                        role,
                    },
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
        }
        rig.org
            .execute(
                OrgCommand::Down {
                    agent_id: "alpha/down".parse().unwrap(),
                    reason: "fixture".into(),
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        rig.org
            .execute(
                OrgCommand::Adopt {
                    agent_id: "alpha/adopted".parse().unwrap(),
                    pane: "%2".into(),
                    session_id: None,
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        for (name, front_matter) in [
            ("allowed", "harness='pi'\nmodel='test-lite:max'"),
            ("both", "harness='pi'\nmodel='test-pro:max'"),
            (
                "broken-line",
                "harness='pi'\nmodel='test-pro:max'\nmanager='missing-manager'",
            ),
            ("default", "harness='pi'"),
            ("failed", "harness=17"),
            ("worker", "harness='pi'\nmodel='test-pro:max'"),
        ] {
            std::fs::write(
                rig._dir.path().join(format!(".atm/roles/{name}.md")),
                format!("+++\n{front_matter}\n+++\n"),
            )
            .unwrap();
        }
        let mut services = MethodServices::new(rig.registry.clone());
        services.org = rig.org.clone();
        services.departments_dir = Some(dir);
        let router = router_with_reporting(services, rig.files.clone()).unwrap();
        let tree = router.call(caller(), "org.tree", json!({})).await.unwrap();
        assert_eq!(tree["departments"][0]["hire_mode"], "report");
        assert_eq!(
            tree["departments"][0]["would_wait"],
            json!([
                {"seat":"alpha/adopted", "reason":"not on roster"},
                {"seat":"alpha/both", "reason":"not on roster; model \"test-pro:max\" is not allowed"},
                {"seat":"alpha/broken-line", "reason":"model \"test-pro:max\" is not allowed"},
                {"seat":"alpha/default", "reason":"model (default) is not allowed"},
                {"seat":"alpha/failed", "reason":"model \"test-pro:max\" is not allowed"},
                {"seat":"alpha/missing", "reason":"model \"test-pro:max\" is not allowed"},
                {"seat":"alpha/worker", "reason":"model \"test-pro:max\" is not allowed"}
            ])
        );
        let broken = tree["projects"][0]["seats"]
            .as_array()
            .unwrap()
            .iter()
            .find(|seat| seat["agent_id"] == "alpha/broken-line")
            .unwrap();
        assert_eq!(
            broken["line_error"],
            "reporting line at alpha/broken-line: unknown manager alpha/missing-manager"
        );
        assert!(broken["manager"].is_null());
    }

    // Break caught: a single retained filesystem wait times out the RPC, or creates more workers.
    #[tokio::test]
    async fn org_tree_department_reader_hang_is_fail_soft() {
        let mut rig = Rig::new(0);
        let dir = rig._dir.path().join("departments");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("tools.toml"),
            "title='Tools'\nprojects=['alpha']\n",
        )
        .unwrap();
        let files = DepartmentFiles::new(Some(dir));
        let mut router = MethodRouter::default();
        register_org_tree_method(
            &mut router,
            rig.org.clone(),
            rig.registry.clone(),
            rig.files.clone(),
            files.clone(),
        )
        .unwrap();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let slot = files.slot.clone();
        let worker = tokio::spawn(async move {
            detached_read(
                slot,
                std::path::Path::new("department test gate"),
                move || {
                    entered.send(()).unwrap();
                    let _ = gate.recv();
                    Ok(())
                },
            )
            .await
        });
        entry.await.unwrap();
        let held_roles = rig.files.slot.clone().acquire_owned().await.unwrap();
        let mut read = Box::pin(router.call(caller(), "org.tree", json!({})));
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        rig.sources().await;
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // Both waits are now armed. All inputs are in-memory or gated, with no sockets.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        let tree = read.await.unwrap();
        assert_eq!(tree["seq"], 2);
        assert_eq!(tree["projects"][0]["seats"][0]["agent_id"], "alpha/worker");
        assert_eq!(tree["reporting"]["state"], "unavailable");
        assert_eq!(tree["departments"], json!([]));
        assert_eq!(
            tree["department_problems"],
            json!(["departments: reader timed out or is busy"])
        );
        assert_eq!(worker.await.unwrap().unwrap_err().0.code, "timeout");
        assert_eq!(files.slot.available_permits(), 0);
        tokio::time::resume();
        release.send(()).unwrap();
        drop(files.slot.clone().acquire_owned().await.unwrap());
        drop(held_roles);
        let next = router.call(caller(), "org.tree", json!({})).await.unwrap();
        assert_eq!(next["departments"][0]["id"], "tools");
        assert_eq!(next["department_problems"], json!([]));
        assert_eq!(next["reporting"]["state"], "ok");
        files.slot.close();
        let stopped = router.call(caller(), "org.tree", json!({})).await.unwrap();
        assert_eq!(
            stopped["department_problems"],
            json!(["departments: reader unavailable"])
        );
        assert_eq!(stopped["reporting"]["state"], "ok");
    }

    async fn poll_pending(mut read: Pin<&mut impl Future>) {
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    // Break caught: either file reader starts only after the other finishes.
    #[tokio::test]
    async fn org_tree_reads_departments_and_reporting_in_parallel() {
        let rig = Rig::new(0);
        let fold = rig.org.snapshot_at().await.unwrap();
        // Reply manually so a serial mutant fails at a fence instead of
        // needing a timeout or relying on actor/worker scheduling.
        let (requests, mut snapshots) = mpsc::channel(1);
        let org = OrgHandle {
            requests,
            ..rig.org.clone()
        };
        let (tx, mut rows) = mpsc::channel(1);
        let (events, _) = broadcast::channel(1);
        let registry = RegistryHandle::new(tx, events);
        let dir = rig._dir.path().join("departments");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("tools.toml"),
            "title='Tools'\nprojects=['alpha']\n",
        )
        .unwrap();
        let files = DepartmentFiles::new(Some(dir));
        let mut router = MethodRouter::default();
        register_org_tree_method(&mut router, org, registry, rig.files.clone(), files.clone())
            .unwrap();

        for (first, second, name) in [
            (&files.slot, &rig.files.slot, "department"),
            (&rig.files.slot, &files.slot, "reporting"),
        ] {
            let first_gate = first.acquire().await.unwrap();
            let second_gate = second.acquire().await.unwrap();
            let mut read = Box::pin(router.call(caller(), "org.tree", json!({})));
            poll_pending(read.as_mut()).await;
            let Request::SnapshotAt(reply) = snapshots.try_recv().expect("base snapshot entered")
            else {
                panic!("expected a snapshot read");
            };
            reply.send(Ok(fold.clone())).unwrap();
            let RegistryCommand::GetAllSessions { respond_to } =
                rows.try_recv().expect("base registry read entered")
            else {
                panic!("expected a registry read");
            };
            respond_to.send(vec![]).unwrap();
            poll_pending(read.as_mut()).await;

            drop(first_gate);
            // Release transfers the permit to a queued reader synchronously;
            // the RPC has not been polled again and the other lane is held.
            assert_eq!(
                first.available_permits(),
                0,
                "{name} read must enter while the other lane is held"
            );
            tokio::select! {
                // The worker returns the permit only after its file read.
                permit = first.acquire() => drop(permit.unwrap()),
                result = &mut read => panic!("RPC finished with the other lane held: {result:?}"),
            }
            drop(second_gate);
            let tree = read.await.unwrap();
            assert_eq!(tree["departments"][0]["id"], "tools");
            assert_eq!(tree["reporting"]["state"], "ok");
            assert_eq!(tree["projects"][0]["seats"][0]["manager"], "owner");
        }
    }

    // Break caught: unplaced work is truncated to make a reply fit, or the
    // method returns a too-large value before the connection can bound its frame.
    #[tokio::test]
    async fn org_tree_refuses_oversized_unplaced_tasks_without_truncation() {
        let rig = Rig::new(1000);
        let router = rig.router();
        let error = router
            .call(caller(), "org.tree", json!({}))
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "refused");
        assert!(error.0.message.contains("512 KiB"));
        assert_eq!(rig.org.snapshot().await.unwrap().tasks().count(), 1000);
    }
}
