//! Fresh owner policy at task admission; never consulted during event replay.
//! File work reuses the context reader's retained, detached single-slot pattern.

use super::{
    agent_context::detached_read, agents::AgentError, message_inbox::caller_address,
    reporting_line::ReportingFiles, tasks::TaskError, OrgHandle,
};
use crate::server::methods::{CallContext, MethodError};
use atm_org::{Address, OrgState, ProjectId, TaskMeasure};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Semaphore;

#[derive(Debug, Clone)]
pub(super) struct IntentFiles {
    slot: Arc<Semaphore>,
}
impl Default for IntentFiles {
    fn default() -> Self {
        Self {
            slot: Arc::new(Semaphore::new(1)),
        }
    }
}
impl IntentFiles {
    pub async fn check(
        &self,
        org: &OrgHandle,
        caller: &CallContext,
        requested: Option<&ProjectId>,
        measure: TaskMeasure,
        reporting: &ReportingFiles,
    ) -> Result<(Address, Option<ProjectId>), MethodError> {
        let state = org.snapshot().await.map_err(TaskError::from)?;
        let who = caller_address(&state, caller)?;
        let Some((project, root)) = project_root(&state, &who, requested)? else {
            return Ok((who, None));
        };
        if let Address::Agent(caller) = &who {
            if caller.project != project {
                let mut failure = None;
                let mut allowed = false;
                for agent in state.agents().filter(|agent| agent.id.project == project) {
                    match reporting.ancestors(state.clone(), agent.id.clone()).await {
                        Ok(ancestors) if ancestors.contains(caller) => {
                            allowed = true;
                            break;
                        }
                        Ok(_) => (),
                        Err(error) if matches!(error.0.code.as_str(), "refused" | "not_found") => {
                            failure.get_or_insert(error);
                        }
                        Err(error) => return Err(error),
                    }
                }
                if !allowed {
                    return Err(failure.unwrap_or_else(|| {
                        TaskError::Refused(format!(
                            "{caller} does not manage a registered agent in {project}"
                        ))
                        .into()
                    }));
                }
            }
        }
        let path = root.join(".atm/intent.md");
        let reading = path.clone();
        detached_read(self.slot.clone(), &path, move || {
            let Some(text) = read_policy(&root, "intent.md", 256 * 1024)? else {
                return Ok(());
            };
            let measures =
                atm_org::parse_intent_measures(&text).map_err(|e| refused(&reading, e))?;
            if !measures.contains(&measure) {
                return Err(refused(&reading, "task measure is not listed"));
            }
            Ok(())
        })
        .await?;
        // Bind creator and project to the same snapshot as authority and policy,
        // even if a pane is rebound while the filesystem read is pending.
        Ok((who, Some(project)))
    }
}

fn project_root(
    state: &OrgState,
    who: &Address,
    requested: Option<&ProjectId>,
) -> Result<Option<(ProjectId, PathBuf)>, TaskError> {
    let own = match who {
        Address::Agent(id) => Some(&id.project),
        _ => None,
    };
    let project = if let Some(id) = requested.or(own) {
        Some(
            state
                .project(id)
                .ok_or_else(|| TaskError::NotFound(format!("unknown project {id}")))?,
        )
    } else {
        let mut projects = state.projects();
        let first = projects.next();
        if projects.next().is_some() {
            return Err(TaskError::InvalidParams(
                "task.create requires project_id when the owner has multiple registered projects"
                    .into(),
            ));
        }
        first
    };
    project
        .map(|p| {
            let root = PathBuf::from(&p.root);
            if !root.is_absolute() {
                return Err(TaskError::Refused(
                    "recorded project root is not absolute".into(),
                ));
            }
            Ok((p.id.clone(), root))
        })
        .transpose()
}

pub(super) fn refused(path: &Path, reason: impl std::fmt::Display) -> AgentError {
    AgentError::Refused(format!("{}: {reason}", path.display()))
}
pub(super) fn file_error(path: &Path, error: io::Error) -> AgentError {
    AgentError::Internal(format!("{}: {error}", path.display()))
}

/// Only missing policy under an existing project means absent policy. Do not follow
/// static symlinks; nonblocking/no-follow open also rejects a final-file swap to
/// a FIFO or link. These same-user checks are not a filesystem sandbox.
pub(super) fn read_policy(
    root: &Path,
    name: &str,
    limit: usize,
) -> Result<Option<String>, AgentError> {
    read_policy_with_error(root, name, limit, |path, error| {
        // Preserve the original absent-policy root check's diagnostics too.
        if path == root {
            refused(root, "project root missing")
        } else {
            file_error(path, error)
        }
    })
}

/// The same bounded reader, with the caller's classification of filesystem errors.
pub(super) fn read_policy_with_error(
    root: &Path,
    name: &str,
    limit: usize,
    on_io_error: impl Fn(&Path, io::Error) -> AgentError,
) -> Result<Option<String>, AgentError> {
    let dir = root.join(".atm");
    for (path, directory) in [(&dir, true), (&dir.join(name), false)] {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if directory {
                    match fs::symlink_metadata(root) {
                        Ok(metadata) if metadata.is_dir() => (),
                        Err(e) if e.kind() != io::ErrorKind::NotFound => {
                            return Err(on_io_error(root, e));
                        }
                        _ => return Err(refused(root, "project root missing")),
                    }
                }
                return Ok(None);
            }
            Err(e) => return Err(on_io_error(path, e)),
            Ok(metadata)
                if if directory {
                    !metadata.is_dir()
                } else {
                    !metadata.is_file()
                } =>
            {
                return Err(refused(path, "not a regular policy file/directory"));
            }
            Ok(_) => (),
        }
    }
    let path = dir.join(name);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|e| on_io_error(&path, e))?;
    if !file
        .metadata()
        .map_err(|e| on_io_error(&path, e))?
        .is_file()
    {
        return Err(refused(&path, "not a regular policy file"));
    }
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| on_io_error(&path, e))?;
    if bytes.len() > limit {
        return Err(refused(&path, format!("policy exceeds {limit} bytes")));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| refused(&path, "policy is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::methods::CallerId;
    use atm_org::OrgCommand;
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    async fn world() -> (tempfile::TempDir, OrgHandle, CancellationToken) {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        for project in ["alpha", "beta", "gamma"] {
            let root = dir.path().join(project);
            fs::create_dir_all(root.join(".atm/roles")).unwrap();
            fs::write(
                root.join(".atm/intent.md"),
                "## How we will know\n- owner\n",
            )
            .unwrap();
            org.execute(
                OrgCommand::RegisterProject {
                    project_id: project.parse().unwrap(),
                    root: root.to_str().unwrap().into(),
                    move_root: false,
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        }
        for (id, pane, manager) in [
            ("alpha/worker", "%1", "gamma/chief"),
            ("beta/worker", "%2", "owner"),
            ("gamma/chief", "%3", "owner"),
        ] {
            org.execute(
                OrgCommand::Adopt {
                    agent_id: id.parse().unwrap(),
                    pane: pane.into(),
                    session_id: None,
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            let (project, name) = id.split_once('/').unwrap();
            fs::write(
                dir.path()
                    .join(project)
                    .join(format!(".atm/roles/{name}.md")),
                format!("+++\nharness='pi'\nmanager='{manager}'\n+++\n"),
            )
            .unwrap();
        }
        (dir, org, cancel)
    }
    fn caller(pane: Option<&str>) -> CallContext {
        CallContext {
            client_id: CallerId("intent-project".into()),
            from_pane: pane.map(|p| crate::server::methods::CallerPane(p.into())),
        }
    }

    // Break caught: a cancelled role read frees its slot or makes owner/own creation depend on that slot.
    #[tokio::test]
    async fn project_admission_shares_the_retained_reporting_reader_and_bypasses_it_when_not_needed(
    ) {
        let (_dir, org, cancel) = world().await;
        let files = IntentFiles::default();
        let reporting = ReportingFiles::default();
        let slot = reporting.slot.clone();
        let (entered, entry) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let waiting = tokio::spawn(async move {
            detached_read(slot, Path::new("gated reporting roles"), move || {
                entered.send(()).unwrap();
                let _ = gate.recv();
                Ok(())
            })
            .await
        });
        entry.await.unwrap();
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        let alpha = "alpha".parse().unwrap();
        for who in [caller(None), caller(Some("%1"))] {
            assert_eq!(
                files
                    .check(&org, &who, Some(&alpha), TaskMeasure::Owner, &reporting)
                    .await
                    .unwrap()
                    .1,
                Some(alpha.clone())
            );
        }
        let before = org.snapshot().await.unwrap();
        let cursor = org.status().unwrap();
        tokio::time::pause();
        let error = files
            .check(
                &org,
                &caller(Some("%3")),
                Some(&alpha),
                TaskMeasure::Owner,
                &reporting,
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "timeout");
        assert_eq!(reporting.slot.available_permits(), 0);
        assert_eq!(org.snapshot().await.unwrap(), before);
        assert_eq!(org.status().unwrap(), cursor);
        tokio::time::resume();
        release.send(()).unwrap();
        drop(reporting.slot.acquire().await.unwrap());
        cancel.cancel();
    }

    // Break caught: a pane rebound during policy I/O changes creator or selected project after admission.
    #[tokio::test]
    async fn creator_and_project_stay_bound_to_the_policy_snapshot_during_pane_rebinding() {
        let (_dir, org, cancel) = world().await;
        let files = IntentFiles::default();
        let permit = files.slot.acquire().await.unwrap();
        let (requests, mut receive) = tokio::sync::mpsc::channel(8);
        let gated_org = OrgHandle {
            requests,
            ..org.clone()
        };
        let reading = files.clone();
        let checking = tokio::spawn(async move {
            reading
                .check(
                    &gated_org,
                    &caller(Some("%1")),
                    None,
                    TaskMeasure::Owner,
                    &ReportingFiles::default(),
                )
                .await
        });
        let super::super::Request::Snapshot(reply) = receive.recv().await.unwrap() else {
            panic!("expected admission snapshot");
        };
        reply.send(org.snapshot().await.unwrap()).unwrap();
        for command in [
            OrgCommand::Down {
                agent_id: "alpha/worker".parse().unwrap(),
                reason: "Rebind fixture".into(),
            },
            OrgCommand::Down {
                agent_id: "beta/worker".parse().unwrap(),
                reason: "Rebind fixture".into(),
            },
            OrgCommand::Adopt {
                agent_id: "beta/worker".parse().unwrap(),
                pane: "%1".into(),
                session_id: None,
            },
        ] {
            org.execute(command, chrono::Utc::now()).await.unwrap();
        }
        drop(permit);
        let (creator, project_id) = checking.await.unwrap().unwrap();
        assert_eq!(creator.to_string(), "alpha/worker");
        assert_eq!(project_id, Some("alpha".parse().unwrap()));
        let created = super::super::tasks::TaskHandle::new(org.clone())
            .mutate(caller(Some("%1")), move |_| {
                atm_org::TaskCommand::Create(atm_org::TaskCreated {
                    project_id,
                    creator,
                    title: "Bound snapshot".into(),
                    kind: atm_org::TaskKind::Work,
                    parent: None,
                    judges: None,
                    measure: TaskMeasure::Owner,
                    budget: None,
                    files: vec![],
                    rests_on: None,
                })
            })
            .await
            .unwrap();
        let state = org.snapshot().await.unwrap();
        let task = state.task(&created.task_id).unwrap();
        assert_eq!(task.creator.to_string(), "alpha/worker");
        assert_eq!(task.project_id, Some("alpha".parse().unwrap()));
        cancel.cancel();
    }

    // Break caught: reporting's permission classification changes intent/rule reader errors.
    #[test]
    fn policy_reader_keeps_its_legacy_permission_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".atm")).unwrap();
        for name in ["intent.md", "rules.md"] {
            let path = dir.path().join(".atm").join(name);
            fs::write(&path, "Private policy").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            let permission = match fs::File::open(&path) {
                Ok(_) => {
                    eprintln!("skipping unreadable policy case: chmod 000 does not prevent this user from opening the file");
                    continue;
                }
                Err(error) => error,
            };
            let expected = format!("{}: {permission}", path.display());
            assert!(
                matches!(read_policy(dir.path(), name, 1024), Err(AgentError::Internal(text)) if text == expected)
            );
        }
    }

    // Break caught: giving each policy call a fresh slot lets cancelled/hung reads
    // multiply workers instead of retaining this resource's single in-flight call.
    #[tokio::test]
    async fn intent_slot_survives_cancelled_reader_and_does_not_block_org() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".atm")).unwrap();
        fs::write(
            dir.path().join(".atm/intent.md"),
            "## How we will know\n- owner\n",
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        org.execute(
            OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: dir.path().to_str().unwrap().into(),
                move_root: false,
            },
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        let files = IntentFiles::default();
        let slot = files.slot.clone();
        let (entered, entry) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let path = dir.path().join(".atm/intent.md");
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
        let caller = CallContext {
            client_id: CallerId("policy-slot".into()),
            from_pane: None,
        };
        tokio::time::pause();
        let error = files
            .check(
                &org,
                &caller,
                None,
                TaskMeasure::Owner,
                &ReportingFiles::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.0.code, "timeout");
        assert_eq!(files.slot.available_permits(), 0);
        assert_eq!(org.snapshot().await.unwrap().tasks().count(), 0);
        cancel.cancel();
        tokio::time::resume();
        release.send(()).unwrap();
        drop(files.slot.acquire().await.unwrap());
    }
}
