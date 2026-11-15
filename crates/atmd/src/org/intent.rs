//! Fresh owner policy at task admission; never consulted during event replay.
//! File work reuses the context reader's retained, detached single-slot pattern.

use super::{
    agent_context::detached_read, agents::AgentError, message_inbox::caller_address,
    tasks::TaskError, OrgHandle,
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
    ) -> Result<Address, MethodError> {
        let state = org.snapshot().await.map_err(TaskError::from)?;
        let who = caller_address(&state, caller)?;
        let Some(root) = project_root(&state, &who, requested)? else {
            return Ok(who);
        };
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
        // Bind the task's creator to the same caller snapshot as its policy,
        // even if a pane is rebound while the filesystem read is pending.
        Ok(who)
    }
}

fn project_root(
    state: &OrgState,
    who: &Address,
    requested: Option<&ProjectId>,
) -> Result<Option<PathBuf>, TaskError> {
    let own = match who {
        Address::Agent(id) => Some(&id.project),
        _ => None,
    };
    if own
        .zip(requested)
        .is_some_and(|(own, requested)| own != requested)
    {
        return Err(TaskError::Refused(
            "a named caller cannot select another project's intent".into(),
        ));
    }
    let project = if let Some(id) = own.or(requested) {
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
            Ok(root)
        })
        .transpose()
}

pub(super) fn refused(path: &Path, reason: impl std::fmt::Display) -> AgentError {
    AgentError::Refused(format!("{}: {reason}", path.display()))
}
pub(super) fn file_error(path: &Path, error: io::Error) -> AgentError {
    AgentError::Internal(format!("{}: {error}", path.display()))
}

/// Only a genuinely missing file/directory means absent policy. Do not follow
/// static symlinks; nonblocking/no-follow open also rejects a final-file swap to
/// a FIFO or link. These same-user checks are not a filesystem sandbox.
pub(super) fn read_policy(
    root: &Path,
    name: &str,
    limit: usize,
) -> Result<Option<String>, AgentError> {
    let dir = root.join(".atm");
    for (path, directory) in [(&dir, true), (&dir.join(name), false)] {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(file_error(path, e)),
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
        .map_err(|e| file_error(&path, e))?;
    if !file.metadata().map_err(|e| file_error(&path, e))?.is_file() {
        return Err(refused(&path, "not a regular policy file"));
    }
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| file_error(&path, e))?;
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
            .check(&org, &caller, None, TaskMeasure::Owner)
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
