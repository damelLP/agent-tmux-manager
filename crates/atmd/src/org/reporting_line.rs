//! Fresh reporting-line facts for admission, never replay or note routing.
//!
//! One retained detached-reader slot is shared by all clones. A hung read holds
//! that slot until it clears; manager authority fails closed in the meantime.
//! Owner/creator authority does not consult this reader. Files are read per
//! admission, not atomically across roles; later edits do not revoke admitted work.

use super::{
    agent_context::{detached_read, manager_address, paths},
    agents::AgentError,
    intent::{file_error, read_policy_with_error},
    tasks::TaskError,
};
use crate::server::methods::MethodError;
use atm_org::{Address, AgentId, OrgState};
use atm_protocol::error_code;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::Path,
    sync::Arc,
};
use tokio::sync::{watch, Semaphore};

/// One pass's immediate edges, validated through owner. An unavailable pass
/// has no edges or line errors; callers still project all recorded seats.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReportingEdges {
    pub reason: Option<String>,
    pub edges: BTreeMap<AgentId, Result<Address, String>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReportingFiles {
    pub(super) slot: Arc<Semaphore>,
}
impl Default for ReportingFiles {
    fn default() -> Self {
        Self {
            slot: Arc::new(Semaphore::new(1)),
        }
    }
}
impl ReportingFiles {
    pub(crate) async fn edges(&self, state: OrgState) -> Result<ReportingEdges, MethodError> {
        match detached_read(
            self.slot.clone(),
            Path::new("organisation reporting roles"),
            move || Ok(read_edges(&state, role_manager)),
        )
        .await
        {
            Ok(result) => result,
            Err(error) if error.0.code == error_code::TIMEOUT => Ok(ReportingEdges {
                reason: Some("reporting role reader timed out or is busy".into()),
                ..Default::default()
            }),
            Err(error) => Err(error),
        }
    }

    pub async fn manager_of(
        &self,
        state: OrgState,
        caller: AgentId,
        assignee: AgentId,
    ) -> Result<Option<AgentId>, TaskError> {
        let ancestors = self
            .ancestors(state, assignee.clone())
            .await
            .map_err(|error| {
                if error.0.code == error_code::NOT_FOUND {
                    TaskError::NotFound(error.0.message)
                } else {
                    TaskError::Refused(error.0.message)
                }
            })?;
        Ok(ancestors.contains(&caller).then_some(assignee))
    }

    /// Strict ancestors, returned only after the entire reached chain validates.
    pub(crate) async fn ancestors(
        &self,
        state: OrgState,
        report: AgentId,
    ) -> Result<BTreeSet<AgentId>, MethodError> {
        let agent = state.agent(&report).ok_or_else(|| {
            MethodError::new(error_code::NOT_FOUND, format!("unknown agent {report}"))
        })?;
        let (path, _) = paths(&state, agent)
            .map_err(|error| MethodError::new(error_code::REFUSED, format!("{report}: {error}")))?;
        let (reading, progress) = watch::channel(report.clone());
        let subject = report.clone();
        detached_read(self.slot.clone(), &path, move || {
            Ok(walk(&state, &subject, |node| {
                reading.send_replace(node.clone());
                role_manager(&state, node).map_err(MethodError::from)
            }))
        })
        .await
        .and_then(std::convert::identity)
        .map_err(|error| {
            MethodError::new(
                &error.0.code,
                format!(
                    "manager authority for {report}, reading {}: {error}",
                    progress.borrow().clone()
                ),
            )
        })
    }
}

fn read_edges(
    state: &OrgState,
    mut read: impl FnMut(&OrgState, &AgentId) -> Result<Address, AgentError>,
) -> Result<ReportingEdges, MethodError> {
    let mut managers = BTreeMap::new();
    for agent in state.agents() {
        let result = read(state, &agent.id).map_err(MethodError::from);
        if let Err(error) = &result {
            if error.0.code != error_code::REFUSED && error.0.code != error_code::NOT_FOUND {
                return Err(MethodError::new(
                    &error.0.code,
                    format!("reporting line at {}: {error}", agent.id),
                ));
            }
        }
        managers.insert(agent.id.clone(), result.map_err(|error| error.0));
    }
    let mut edges = BTreeMap::new();
    for agent in state.agents() {
        let manager = |id: &AgentId| {
            managers
                .get(id)
                .cloned()
                .ok_or_else(|| {
                    MethodError::new(error_code::NOT_FOUND, format!("unknown agent {id}"))
                })?
                .map_err(MethodError)
        };
        let edge = walk(state, &agent.id, manager)
            .and_then(|_| manager(&agent.id))
            .map_err(|error| error.0.message);
        edges.insert(agent.id.clone(), edge);
    }
    Ok(ReportingEdges {
        reason: None,
        edges,
    })
}

fn role_manager(state: &OrgState, node: &AgentId) -> Result<Address, AgentError> {
    let agent = state
        .agent(node)
        .ok_or_else(|| AgentError::NotFound(format!("unknown agent {node}")))?;
    // paths validates the recorded root; never resolve from cwd.
    paths(state, agent)?;
    let project = state
        .project(&node.project)
        .ok_or_else(|| AgentError::NotFound(format!("unknown project {}", node.project)))?;
    let role = read_policy_with_error(
        Path::new(&project.root),
        &format!("roles/{}.md", node.name),
        256 * 1024,
        role_file_error,
    )?
    .map(|text| {
        atm_org::parse_role(&node.name, &text)
            .map_err(|_| AgentError::Refused("invalid role configuration".into()))
    })
    .transpose()?;
    manager_address(node, role.as_ref())
        .ok_or_else(|| AgentError::Refused("invalid role manager".into()))
}

fn walk(
    state: &OrgState,
    subject: &AgentId,
    mut manager: impl FnMut(&AgentId) -> Result<Address, MethodError>,
) -> Result<BTreeSet<AgentId>, MethodError> {
    let mut node = subject.clone();
    let mut visited = BTreeSet::from([subject.clone()]);
    loop {
        match manager(&node).and_then(|manager| {
            next_manager(state, &mut visited, manager).map_err(MethodError::from)
        }) {
            Ok(Some(manager)) => node = manager,
            Ok(None) => {
                visited.remove(subject);
                return Ok(visited);
            }
            Err(error) => {
                return Err(MethodError::new(
                    &error.0.code,
                    format!("reporting line at {node}: {error}"),
                ));
            }
        }
    }
}

fn role_file_error(path: &Path, error: io::Error) -> AgentError {
    let configuration = matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::IsADirectory
    ) || error.raw_os_error() == Some(libc::ELOOP);
    let error = file_error(path, error);
    if configuration {
        AgentError::Refused(error.to_string())
    } else {
        error
    }
}

/// One pure edge: only owner terminates; every named node must be registered
/// and new. The caller is not granted anything until the full walk terminates.
fn next_manager(
    state: &OrgState,
    visited: &mut BTreeSet<AgentId>,
    manager: Address,
) -> Result<Option<AgentId>, AgentError> {
    match manager {
        Address::Owner => Ok(None),
        Address::Agent(id) if state.agent(&id).is_none() => {
            Err(AgentError::Refused(format!("unknown manager {id}")))
        }
        Address::Agent(id) if !visited.insert(id.clone()) => {
            Err(AgentError::Refused(format!("reporting cycle through {id}")))
        }
        Address::Agent(id) => Ok(Some(id)),
        Address::System => Err(AgentError::Refused("system cannot be a manager".into())),
    }
}

#[cfg(test)]
#[path = "reporting_edges_tests.rs"]
mod edges_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use atm_org::{OrgCommand, OrgEvent};
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    // Break caught: a shared walk changes full-chain validation, relative
    // resolution or the node identified by a sanitized refusal.
    #[tokio::test]
    async fn reporting_walk_characterizes_current_authority() {
        let (dir, mut state) = fixture(3);
        let report: AgentId = "alpha/worker".parse().unwrap();
        state
            .apply(
                100,
                chrono::Utc::now(),
                &OrgEvent::AgentDown {
                    agent_id: "alpha/lead".parse().unwrap(),
                    reason: "Fixture".into(),
                },
            )
            .unwrap();
        let mut seen = Vec::new();
        let ancestors = walk(&state, &report, |node| {
            seen.push(node.to_string());
            role_manager(&state, node).map_err(MethodError::from)
        })
        .unwrap();
        assert_eq!(ancestors, ids(&["alpha/lead", "gamma/chief"]));
        assert_eq!(seen, ["alpha/worker", "alpha/lead", "gamma/chief"]);
        for (manager, diagnostic) in [
            ("alpha/worker", "reporting cycle through alpha/worker"),
            ("beta/unknown", "unknown manager beta/unknown"),
            ("../invalid", "invalid role manager"),
        ] {
            set_manager(dir.path(), "gamma/chief", manager);
            let error = walk(&state, &report, |node| {
                role_manager(&state, node).map_err(MethodError::from)
            })
            .unwrap_err();
            assert_eq!(error.0.code, "refused");
            assert_eq!(
                error.0.message,
                format!("reporting line at gamma/chief: {diagnostic}")
            );
            assert!(!error.0.message.contains("Private instructions"));
        }
        let error = walk(&state, &report, |_| Ok(Address::System)).unwrap_err();
        assert_eq!(
            error.0.message,
            "reporting line at alpha/worker: system cannot be a manager"
        );
        for text in [
            None,
            Some("+++\nharness='pi'\n+++\nPrivate instructions\n"),
            Some("+++\nharness='pi'\nmanager='owner'\n+++\n"),
        ] {
            match text {
                Some(text) => std::fs::write(role_path(dir.path(), "alpha/worker"), text).unwrap(),
                None => std::fs::remove_file(role_path(dir.path(), "alpha/worker")).unwrap(),
            }
            assert_eq!(role_manager(&state, &report).unwrap(), Address::Owner);
            assert!(walk(&state, &report, |node| role_manager(&state, node)
                .map_err(MethodError::from))
            .unwrap()
            .is_empty());
            assert!(ReportingFiles::default()
                .ancestors(state.clone(), report.clone())
                .await
                .unwrap()
                .is_empty());
        }
    }

    // Break caught: non-configuration I/O is reported as a safely omittable broken chain.
    #[test]
    fn role_io_errors_keep_infrastructure_distinct_from_configuration() {
        let path = Path::new("/alpha/.atm/roles/worker.md");
        for (code, expected) in [
            (libc::EACCES, "refused"),
            (libc::EPERM, "refused"),
            (libc::ENOTDIR, "refused"),
            (libc::EISDIR, "refused"),
            (libc::ELOOP, "refused"),
            (libc::EMFILE, "internal"),
            (libc::ENFILE, "internal"),
            (libc::EIO, "internal"),
            (libc::ENOMEM, "internal"),
            (libc::ESTALE, "internal"),
        ] {
            let error = std::io::Error::from_raw_os_error(code);
            let text = format!("agent operation failed: {}: {error}", path.display());
            let actual = MethodError::from(role_file_error(path, error));
            assert_eq!(actual.0.code, expected, "OS error {code}");
            assert_eq!(
                actual.0.message, text,
                "preserve the legacy task diagnostic"
            );
        }
    }

    // Synthetic: configuration, not repository or seat names, determines ancestry.
    fn fixture(count: usize) -> (tempfile::TempDir, OrgState) {
        let dir = tempfile::tempdir().unwrap();
        let mut state = OrgState::default();
        let mut seq = 0;
        let mut apply = |event| {
            seq += 1;
            state.apply(seq, chrono::Utc::now(), &event).unwrap();
        };
        for project in ["alpha", "beta", "gamma", "delta", "epsilon"]
            .into_iter()
            .take(count)
        {
            let root = dir.path().join(project);
            std::fs::create_dir_all(root.join(".atm/roles")).unwrap();
            apply(OrgEvent::ProjectRegistered {
                project_id: project.parse().unwrap(),
                root: root.to_str().unwrap().into(),
            });
        }
        for (id, pane, manager) in [
            ("alpha/worker", "%1", "lead"),
            ("alpha/lead", "%2", "gamma/chief"),
            ("gamma/chief", "%3", "owner"),
        ] {
            apply(OrgEvent::AgentAdopted {
                agent_id: id.parse().unwrap(),
                pane: pane.into(),
                session_id: None,
            });
            set_manager(dir.path(), id, manager);
        }
        assert!(!dir.path().join("gamma/.git").exists());
        (dir, state)
    }
    fn role_path(root: &Path, id: &str) -> std::path::PathBuf {
        let (project, name) = id.split_once('/').unwrap();
        root.join(project).join(format!(".atm/roles/{name}.md"))
    }
    fn set_manager(root: &Path, id: &str, manager: &str) {
        std::fs::write(
            role_path(root, id),
            format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nPrivate instructions\n"),
        )
        .unwrap();
    }
    fn ids(names: &[&str]) -> BTreeSet<AgentId> {
        names.iter().map(|id| id.parse().unwrap()).collect()
    }

    // Break caught: returning partial ancestors before validating a later broken edge.
    #[tokio::test]
    async fn reporting_ancestors_validate_the_whole_chain() {
        for count in [3, 5] {
            let (dir, mut state) = fixture(count);
            state
                .apply(
                    100,
                    chrono::Utc::now(),
                    &OrgEvent::AgentDown {
                        agent_id: "alpha/lead".parse().unwrap(),
                        reason: "Fixture".into(),
                    },
                )
                .unwrap();
            let files = ReportingFiles::default();
            let report: AgentId = "alpha/worker".parse().unwrap();
            assert_eq!(
                files
                    .ancestors(state.clone(), report.clone())
                    .await
                    .unwrap(),
                ids(&["alpha/lead", "gamma/chief"])
            );
            assert_eq!(
                files
                    .ancestors(state.clone(), "alpha/lead".parse().unwrap())
                    .await
                    .unwrap(),
                ids(&["gamma/chief"])
            );
            assert!(files
                .ancestors(state.clone(), "gamma/chief".parse().unwrap())
                .await
                .unwrap()
                .is_empty());
            for (node, manager) in [
                ("alpha/worker", "alpha/worker"),
                ("alpha/lead", "alpha/worker"),
                ("gamma/chief", "alpha/lead"),
                ("gamma/chief", "beta/unknown"),
                ("gamma/chief", "../invalid"),
            ] {
                set_manager(dir.path(), node, manager);
                let error = files
                    .ancestors(state.clone(), report.clone())
                    .await
                    .unwrap_err();
                assert_eq!(error.0.code, "refused");
                assert!(error.0.message.contains(node), "{error}");
                assert!(!error.0.message.contains("Private instructions"));
                set_manager(dir.path(), "alpha/worker", "lead");
                set_manager(dir.path(), "alpha/lead", "gamma/chief");
                set_manager(dir.path(), "gamma/chief", "owner");
            }
            std::fs::remove_file(role_path(dir.path(), "alpha/lead")).unwrap();
            assert_eq!(
                files
                    .ancestors(state.clone(), report.clone())
                    .await
                    .unwrap(),
                ids(&["alpha/lead"])
            );
            std::fs::remove_file(role_path(dir.path(), "alpha/worker")).unwrap();
            assert!(files.ancestors(state, report).await.unwrap().is_empty());
        }
    }

    // Break caught: cancelled waiters release the detached worker's slot or block the writer.
    #[tokio::test]
    async fn reporting_ancestors_retain_one_slot_after_cancellation() {
        use std::{future::Future, task::Poll, time::Duration};
        let (dir, state) = fixture(3);
        let files = ReportingFiles::default();
        let slot = files.slot.clone();
        let path = role_path(dir.path(), "alpha/worker");
        let (entered, entry) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let held = tokio::spawn(async move {
            detached_read(slot, &path, move || {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                Ok(())
            })
            .await
        });
        entry.await.unwrap();
        held.abort();
        assert!(held.await.unwrap_err().is_cancelled());
        assert_eq!(files.slot.available_permits(), 0);
        let clone = files.clone();
        let mut ancestors =
            Box::pin(files.ancestors(state.clone(), "alpha/worker".parse().unwrap()));
        let mut manager = Box::pin(clone.manager_of(
            state.clone(),
            "alpha/lead".parse().unwrap(),
            "alpha/worker".parse().unwrap(),
        ));
        std::future::poll_fn(|cx| {
            assert!(ancestors.as_mut().poll(cx).is_pending());
            assert!(manager.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let cancel = CancellationToken::new();
        let org = super::super::OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        org.execute(
            OrgCommand::RegisterProject {
                project_id: "alpha".parse().unwrap(),
                root: dir.path().to_str().unwrap().into(),
                move_root: false,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(ancestors.await.unwrap_err().0.code, "timeout");
        assert!(matches!(manager.await, Err(TaskError::Refused(_))));
        assert_eq!(files.slot.available_permits(), 0);
        tokio::time::resume();
        release.send(()).unwrap();
        drop(files.slot.acquire().await.unwrap());
        assert_eq!(
            files
                .ancestors(state, "alpha/worker".parse().unwrap())
                .await
                .unwrap(),
            ids(&["alpha/lead", "gamma/chief"])
        );
        cancel.cancel();
    }

    struct ReadRig {
        dir: tempfile::TempDir,
        org: super::super::OrgHandle,
        messages: super::super::messages::MessageHandle,
        runtime: super::super::messages::MessageRuntime,
        files: ReportingFiles,
        cancel: CancellationToken,
    }
    impl Drop for ReadRig {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }
    impl ReadRig {
        async fn new() -> Self {
            use crate::eventlog::Envelope;
            use std::sync::atomic::{AtomicU64, Ordering};
            use tokio::sync::{broadcast, mpsc, watch};
            let (dir, _) = fixture(3);
            let cancel = CancellationToken::new();
            let (requests, receiver) = mpsc::channel(64);
            let (health, status) = watch::channel(Ok(0));
            let (committed, _) = broadcast::channel(1024);
            let next = AtomicU64::new(0);
            tokio::spawn(super::super::actor::run(
                OrgState::default(),
                receiver,
                health,
                committed.clone(),
                cancel.clone(),
                move |_: Envelope| {
                    let seq = next.fetch_add(1, Ordering::SeqCst) + 1;
                    async move { Ok(seq) }
                },
            ));
            let org = super::super::OrgHandle {
                requests,
                health: status,
                committed: committed.clone(),
                reads: None,
            };
            let runtime = super::super::message_service_tests::runtime(org.clone(), 0, &cancel);
            let (messages, _) = super::super::messages::spawn_service_for_test(
                runtime.clone(),
                committed.subscribe(),
                0,
                None,
            );
            tokio::time::timeout(std::time::Duration::from_secs(60), messages.initialized())
                .await
                .unwrap()
                .unwrap();
            let rig = Self {
                dir,
                org,
                messages,
                runtime,
                files: ReportingFiles::default(),
                cancel,
            };
            for project in ["alpha", "beta", "gamma"] {
                rig.execute(OrgCommand::RegisterProject {
                    project_id: project.parse().unwrap(),
                    root: rig.dir.path().join(project).to_str().unwrap().into(),
                    move_root: false,
                })
                .await;
            }
            for (id, pane) in [
                ("alpha/worker", "%1"),
                ("alpha/lead", "%2"),
                ("gamma/chief", "%3"),
            ] {
                rig.execute(OrgCommand::Adopt {
                    agent_id: id.parse().unwrap(),
                    pane: pane.into(),
                    session_id: None,
                })
                .await;
            }
            rig
        }
        async fn hold(&self) -> std::sync::mpsc::Sender<()> {
            let slot = self.files.slot.clone();
            let path = role_path(self.dir.path(), "alpha/worker");
            let (entered, entry) = oneshot::channel();
            let (release, gate) = std::sync::mpsc::channel();
            let waiting = tokio::spawn(async move {
                detached_read(slot, &path, move || {
                    entered.send(()).unwrap();
                    let _ = gate.recv();
                    Ok(())
                })
                .await
            });
            entry.await.unwrap();
            waiting.abort();
            assert!(waiting.await.unwrap_err().is_cancelled());
            release
        }
        async fn execute(&self, command: OrgCommand) -> u64 {
            let events = self.org.execute(command, chrono::Utc::now()).await.unwrap();
            let seq = events.last().unwrap().seq;
            self.runtime.history.folded(seq).await.unwrap();
            seq
        }
        async fn send(&self, text: &str) -> atm_org::MessageId {
            let result = self
                .messages
                .send(
                    serde_json::from_value(serde_json::json!({"to":"alpha/worker","body":text}))
                        .unwrap(),
                    call(None),
                )
                .await
                .unwrap();
            result.message_id
        }
        async fn task(&self, creator: &str) -> String {
            let seq = self
                .execute(OrgCommand::Task(atm_org::TaskCommand::Create(
                    atm_org::TaskCreated {
                        project_id: None,
                        title: "Work".into(),
                        kind: atm_org::TaskKind::Work,
                        creator: creator.parse().unwrap(),
                        parent: None,
                        judges: None,
                        measure: atm_org::TaskMeasure::Mechanical {
                            command: "check".into(),
                        },
                        budget: None,
                        files: vec![],
                        rests_on: None,
                    },
                )))
                .await;
            let id = format!("t{seq}");
            self.execute(OrgCommand::Task(atm_org::TaskCommand::Assign {
                task_id: id.parse().unwrap(),
                agent_id: "alpha/worker".parse().unwrap(),
                by: atm_org::TaskActor {
                    who: Address::Owner,
                    manager_of: None,
                },
            }))
            .await;
            id
        }
    }
    fn call(pane: Option<&str>) -> crate::server::methods::CallContext {
        crate::server::methods::CallContext {
            client_id: crate::server::methods::CallerId("reporting-reader".into()),
            from_pane: pane.map(|pane| crate::server::methods::CallerPane(pane.into())),
        }
    }

    // Break caught: holding the view across role await, or re-snapshotting after admission.
    #[tokio::test]
    async fn manager_lookup_does_not_hold_message_projection() {
        use std::{future::Future, task::Poll};
        let rig = ReadRig::new().await;
        let first = rig.send("SECRET old body").await;
        let messages = rig.messages.clone().with_reporting_files(rig.files.clone());
        let release = rig.hold().await;
        let params = serde_json::from_value(serde_json::json!({"for":"alpha/worker"})).unwrap();
        let mut lookup = Box::pin(messages.inbox(params, call(Some("%2"))));
        std::future::poll_fn(|cx| {
            assert!(lookup.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        rig.send("SECRET newer body").await;
        for pane in [None, Some("%1")] {
            let page = rig
                .messages
                .inbox(
                    serde_json::from_value(serde_json::json!({"for":"alpha/worker"})).unwrap(),
                    call(pane),
                )
                .await
                .unwrap();
            assert_eq!(
                page.messages.len(),
                2,
                "fold and owner/self reads must complete while lookup is held"
            );
        }
        let root = rig.dir.path().join("new-alpha");
        std::fs::create_dir_all(root.join(".atm/roles")).unwrap();
        std::fs::write(
            root.join(".atm/roles/worker.md"),
            "+++\nharness='pi'\nmanager='owner'\n+++\nNew role\n",
        )
        .unwrap();
        rig.execute(OrgCommand::RegisterProject {
            project_id: "alpha".parse().unwrap(),
            root: root.to_str().unwrap().into(),
            move_root: true,
        })
        .await;
        rig.execute(OrgCommand::Down {
            agent_id: "alpha/worker".parse().unwrap(),
            reason: "Fixture rebind".into(),
        })
        .await;
        rig.execute(OrgCommand::Adopt {
            agent_id: "alpha/worker".parse().unwrap(),
            pane: "%9".into(),
            session_id: None,
        })
        .await;
        release.send(()).unwrap();
        let page = lookup.await.unwrap();
        assert_eq!(
            page.messages.len(),
            1,
            "admission must project its captured snapshot"
        );
        assert_eq!(page.messages[0].message_id, first);
        assert!(page.messages[0].content.body.is_empty());
        let error = messages
            .inbox(
                serde_json::from_value(serde_json::json!({"for":"alpha/worker"})).unwrap(),
                call(Some("%2")),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, super::super::messages::MessageError::Refused(_)),
            "next call must see the new root/role"
        );
    }

    // Break caught: router families get independent slots, or a timed-out list returns partial counts.
    #[tokio::test]
    async fn reporting_reader_is_shared_across_method_families() {
        use crate::server::methods::{router_with_reporting, MethodServices};
        use serde_json::json;
        use std::{future::Future, task::Poll, time::Duration};
        let rig = ReadRig::new().await;
        rig.send("SECRET body").await;
        let task = rig.task("owner").await;
        let own_task = rig.task("alpha/lead").await;
        let mut request: atm_org::DecisionRequest = serde_json::from_value(json!({"requester":"alpha/worker","kind":"plan","title":"Choose","context":"Full context","files":[],"options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true})).unwrap();
        let child = format!(
            "d{}",
            rig.execute(OrgCommand::Decision(atm_org::DecisionCommand::Request(
                request.clone()
            )))
            .await
        );
        request.requester = "alpha/lead".parse().unwrap();
        let own = format!(
            "d{}",
            rig.execute(OrgCommand::Decision(atm_org::DecisionCommand::Request(
                request
            )))
            .await
        );
        // Reads remain supported even with the answer bridge unavailable.
        let mut decisions =
            super::super::decisions::DecisionHandle::disabled("not needed for reads");
        decisions.org = rig.org.clone();
        let mut services = MethodServices::new(crate::registry::spawn_registry());
        services.org = rig.org.clone();
        services.messages = Some(rig.messages.clone());
        services.decisions = Some(decisions);
        let router = router_with_reporting(services, rig.files.clone()).unwrap();
        let release = rig.hold().await;
        let mut task_read = Box::pin(router.call(
            call(Some("%2")),
            "task.set_state",
            json!({"task_id":task,"state":"cancelled"}),
        ));
        let mut decision_read = Box::pin(router.call(call(Some("%2")), "decision.list", json!({})));
        let mut inbox_read =
            Box::pin(router.call(call(Some("%2")), "msg.inbox", json!({"for":"alpha/worker"})));
        std::future::poll_fn(|cx| {
            assert!(task_read.as_mut().poll(cx).is_pending());
            assert!(decision_read.as_mut().poll(cx).is_pending());
            assert!(inbox_read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // Explicit operations/folds synchronize; none needs the held role slot.
        router
            .call(
                call(None),
                "task.set_state",
                json!({"task_id":task,"state":"cancelled"}),
            )
            .await
            .unwrap();
        router
            .call(
                call(Some("%2")),
                "task.set_state",
                json!({"task_id":own_task,"state":"cancelled"}),
            )
            .await
            .unwrap();
        for (pane, params) in [
            (None, json!({"for":"alpha/worker"})),
            (Some("%1"), json!({})),
            (Some("%2"), json!({"for":"owner","count_only":true})),
        ] {
            router.call(call(pane), "msg.inbox", params).await.unwrap();
        }
        router
            .call(call(None), "decision.get", json!({"id":child}))
            .await
            .unwrap();
        router
            .call(call(Some("%2")), "decision.get", json!({"id":own}))
            .await
            .unwrap();
        router
            .call(
                call(Some("%2")),
                "decision.list",
                json!({"count_only":true}),
            )
            .await
            .unwrap();
        std::future::poll_fn(|cx| {
            assert!(decision_read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(rig.files.slot.available_permits(), 0);
        // All remaining activity is in-memory actor/slot/clock work, not sockets or files.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            decision_read.await.unwrap_err().0.code,
            "timeout",
            "never return successful partial own-only rows/counts"
        );
        assert_eq!(inbox_read.await.unwrap_err().0.code, "timeout");
        assert_eq!(task_read.await.unwrap_err().0.code, "refused");
        assert_eq!(rig.files.slot.available_permits(), 0);
        tokio::time::resume();
        release.send(()).unwrap();
        drop(rig.files.slot.acquire().await.unwrap());
        let page = router
            .call(call(Some("%2")), "decision.list", json!({}))
            .await
            .unwrap();
        assert_eq!(page["decisions"].as_array().unwrap().len(), 2);

        // A closed lane is an event, not elapsed time: an independent reader
        // would succeed (or report its own failure), never this unavailable
        // slot. This also prevents paused time from masking a stray file read.
        let release = rig.hold().await;
        let mut closed_list = Box::pin(router.call(call(Some("%2")), "decision.list", json!({})));
        let mut closed_inbox =
            Box::pin(router.call(call(Some("%2")), "msg.inbox", json!({"for":"alpha/worker"})));
        let mut closed_task = Box::pin(router.call(
            call(Some("%2")),
            "task.set_state",
            json!({"task_id":task,"state":"cancelled"}),
        ));
        std::future::poll_fn(|cx| {
            assert!(closed_list.as_mut().poll(cx).is_pending());
            assert!(closed_inbox.as_mut().poll(cx).is_pending());
            assert!(closed_task.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        rig.files.slot.close();
        assert_eq!(closed_list.await.unwrap_err().0.code, "unavailable");
        assert_eq!(closed_inbox.await.unwrap_err().0.code, "unavailable");
        assert_eq!(closed_task.await.unwrap_err().0.code, "refused");
        release.send(()).unwrap();
    }
}
