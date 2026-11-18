//! One bounded fold/registry/file join, shared by every organisation-tree caller.
use super::{agents::AgentError, org_tree::project, reporting_line::ReportingFiles, OrgHandle};
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
) -> Result<(), MethodError> {
    router.register("org.tree", Duration::from_secs(12), move |_, params| {
        let (org, registry, reporting) = (org.clone(), registry.clone(), reporting.clone());
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
            let ((state, seq), rows) = tokio::try_join!(snapshot, live)?;
            let edges = reporting.edges(state.clone()).await?;
            let value =
                serde_json::to_value(project(&state, seq, &rows, &edges, chrono::Utc::now()))
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
        org::Request,
        registry::RegistryCommand,
        server::methods::{router_with_reporting, CallContext, CallerId, MethodServices},
    };
    use atm_org::{OrgCommand, OrgEvent, OrgState};
    use serde_json::json;
    use std::{
        future::Future,
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
