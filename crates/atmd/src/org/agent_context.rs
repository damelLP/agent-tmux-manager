//! Read-only agent entry context. Files are fresh, never a saved-role fallback.
//! A missing role is explicit, not fatal; an invalid role is never trusted.

use super::{agents::AgentError, message_inbox::caller_address, messages::MessageError, OrgHandle};
use crate::server::methods::{parse_params, CallContext, MethodError, MethodRouter};
use atm_org::{Address, Agent, AgentId, MessageId, MessageKind, OrgState, RoleSpec};
use atm_protocol::error_code;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{oneshot, Semaphore};

const ROLE_BYTES: usize = 256 * 1024;
const CONTEXT_BYTES: usize = 512 * 1024;
const FILE_BUDGET: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMessage {
    pub message_id: MessageId,
    pub kind: MessageKind,
    pub from: Address,
    pub first_line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFile {
    pub path: PathBuf,
    pub exists: bool,
}

/// `open_tasks` is reserved for task summaries; E1 always returns an empty array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    pub agent_id: AgentId,
    pub manager: Address,
    pub role_file: PathBuf,
    pub instructions: Option<String>,
    pub unread_messages: Vec<ContextMessage>,
    pub memory: MemoryFile,
    pub open_tasks: Vec<serde_json::Value>,
    pub report_command: String,
}

fn context_agent<'s>(state: &'s OrgState, caller: &CallContext) -> Result<&'s Agent, MessageError> {
    let Address::Agent(id) = caller_address(state, caller)? else {
        return Err(MessageError::Refused(
            "atm me requires a named agent bound to the caller's pane".into(),
        ));
    };
    state
        .agent(&id)
        .ok_or_else(|| MessageError::NotFound(format!("agent {id} was not found")))
}

pub(super) fn paths(state: &OrgState, agent: &Agent) -> Result<(PathBuf, PathBuf), AgentError> {
    let project = state.project(&agent.id.project).ok_or_else(|| {
        AgentError::NotFound(format!("project {} was not found", agent.id.project))
    })?;
    let root = Path::new(&project.root);
    if !root.is_absolute() {
        return Err(AgentError::Refused(
            "recorded project root is not absolute".into(),
        ));
    }
    let name = format!("{}.md", agent.id.name);
    Ok((
        root.join(".atm/roles").join(&name),
        root.join(".atm/memory").join(name),
    ))
}

/// The address a role names as its manager: the owner when it names none or
/// `owner`, an agent of the same project for a bare name, the address as
/// written for a full id. `None` when the name is not a valid address.
pub(super) fn manager_address(agent: &AgentId, role: Option<&RoleSpec>) -> Option<Address> {
    match role.and_then(|r| r.config.manager.as_deref()) {
        None | Some("owner") => Some(Address::Owner),
        Some(name) if name.contains('/') => name.parse::<Address>().ok(),
        Some(name) => AgentId::new(agent.project.clone(), name)
            .map(Address::Agent)
            .ok(),
    }
}

fn assemble_context(
    state: &OrgState,
    agent: &Agent,
    role: Option<RoleSpec>,
    memory_exists: bool,
) -> Result<AgentContext, AgentError> {
    let (role_file, memory_path) = paths(state, agent)?;
    let manager = manager_address(&agent.id, role.as_ref()).ok_or_else(|| {
        AgentError::Refused(format!("{}: invalid role manager", role_file.display()))
    })?;
    let unread_messages = state
        .inbox(&Address::Agent(agent.id.clone()))
        .into_iter()
        .filter(|message| message.read_at.is_none())
        .map(|message| ContextMessage {
            message_id: message.id,
            kind: message.content.kind,
            from: message.content.from.clone(),
            first_line: message
                .content
                .body
                .lines()
                .next()
                .unwrap_or_default()
                .into(),
        })
        .collect();
    Ok(AgentContext {
        agent_id: agent.id.clone(),
        report_command: format!(
            "atm msg send --to {manager} --kind report --body 'DONE: <result>'"
        ),
        manager,
        role_file,
        instructions: role.map(|r| r.instructions),
        unread_messages,
        memory: MemoryFile {
            path: memory_path,
            exists: memory_exists,
        },
        open_tasks: Vec::new(),
    })
}

fn file_error(path: &Path, error: impl std::fmt::Display) -> AgentError {
    AgentError::Internal(format!("{}: {error}", path.display()))
}

fn read_files(
    role_file: &Path,
    memory_file: &Path,
    name: &str,
) -> Result<(Option<RoleSpec>, bool), AgentError> {
    let nonregular = || {
        AgentError::Refused(format!(
            "{}: role is not a regular file",
            role_file.display()
        ))
    };
    match fs::metadata(role_file) {
        Ok(metadata) if !metadata.is_file() => return Err(nonregular()),
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(file_error(role_file, error));
        }
        _ => {}
    }
    let role = match fs::File::open(role_file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(file_error(role_file, error)),
        Ok(file) => {
            if !file
                .metadata()
                .map_err(|e| file_error(role_file, e))?
                .is_file()
            {
                return Err(nonregular());
            }
            let mut bytes = Vec::new();
            file.take(ROLE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| file_error(role_file, e))?;
            if bytes.len() > ROLE_BYTES {
                return Err(AgentError::Refused(format!(
                    "{}: role exceeds {ROLE_BYTES} bytes",
                    role_file.display()
                )));
            }
            let invalid = || {
                AgentError::Refused(format!(
                    "{}: invalid role configuration",
                    role_file.display()
                ))
            };
            let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
            Some(atm_org::parse_role(name, text).map_err(|_| invalid())?)
        }
    };
    let exists = match fs::metadata(memory_file) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => {
            return Err(AgentError::Refused(format!(
                "{}: memory is not a regular file",
                memory_file.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(file_error(memory_file, error)),
    };
    Ok((role, exists))
}

/// A worker owns the slot, not its waiting future. Timeouts/cancellation cannot
/// multiply stalled filesystem threads, and Tokio never joins a hung read.
pub(super) async fn detached_read<T: Send + 'static>(
    slot: Arc<Semaphore>,
    role_file: &Path,
    read: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
) -> Result<T, MethodError> {
    tokio::time::timeout(FILE_BUDGET, async move {
        let permit = slot
            .acquire_owned()
            .await
            .map_err(|_| AgentError::Unavailable("context reader stopped".into()))?;
        let (send, receive) = oneshot::channel();
        std::thread::Builder::new()
            .name("atm-agent-context".into())
            .spawn(move || {
                let result = read();
                drop(permit);
                let _ = send.send(result);
            })
            .map_err(|e| AgentError::Internal(format!("context reader: {e}")))?;
        receive
            .await
            .map_err(|_| AgentError::Unavailable("context reader stopped".into()))?
    })
    .await
    .map_err(|_| {
        MethodError::new(
            error_code::TIMEOUT,
            format!(
                "reading agent context files for {} took longer than 5 s",
                role_file.display()
            ),
        )
    })?
    .map_err(MethodError::from)
}

/// Advertised even if persistence is unavailable; no lifecycle/message runtime
/// is needed for this read-only projection of committed state.
pub fn register_agent_context_method(
    router: &mut MethodRouter,
    org: OrgHandle,
) -> Result<(), MethodError> {
    let slot = Arc::new(Semaphore::new(1));
    router.register("agent.context", Duration::from_secs(12), move |caller, params| {
        let org = org.clone();
        let slot = slot.clone();
        async move {
            parse_params::<ContextParams>(params)?;
            let state = org.snapshot().await.map_err(AgentError::from)?;
            let agent = context_agent(&state, &caller)?;
            let (role_file, memory_file) = paths(&state, agent)?;
            let name = agent.id.name.clone();
            let reading = role_file.clone();
            let (role, exists) = detached_read(slot, &reading, move || read_files(&role_file, &memory_file, &name)).await?;
            let context = assemble_context(&state, agent, role, exists)?;
            let value = serde_json::to_value(context).map_err(|e| MethodError::new(error_code::INTERNAL, e.to_string()))?;
            let bytes = serde_json::to_vec(&value).map_err(|e| MethodError::new(error_code::INTERNAL, e.to_string()))?.len();
            if bytes > CONTEXT_BYTES {
                return Err(MethodError::new(error_code::REFUSED, format!("agent context exceeds {CONTEXT_BYTES} bytes; use atm inbox --unread for messages")));
            }
            Ok(value)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::{oneshot, Semaphore};

    // Break caught: assembly consulting a cached role or doing cwd-dependent
    // path resolution instead of using the supplied state/role/file facts.
    #[test]
    fn context_assembly_uses_supplied_role_and_file_facts_without_mutation() {
        use atm_org::{MessageContent, OrgEvent};
        let now = "2026-01-15T12:00:00Z".parse().unwrap();
        let id: AgentId = "project/worker".parse().unwrap();
        let mut state = OrgState::default();
        for (seq, event) in [
            OrgEvent::ProjectRegistered {
                project_id: id.project.clone(),
                root: "/project".into(),
            },
            OrgEvent::AgentUpRequested {
                agent_id: id.clone(),
                role: atm_org::parse_role("worker", "+++\nharness='pi'\n+++\nStale").unwrap(),
            },
            OrgEvent::AgentPaneCreated {
                agent_id: id.clone(),
                pane: "%7".into(),
            },
            OrgEvent::MessageSent(MessageContent {
                from: Address::System,
                to: Address::Agent(id.clone()),
                kind: MessageKind::Note,
                body: "First\nSecond".into(),
                attachments: vec![],
                task: None,
            }),
        ]
        .into_iter()
        .enumerate()
        {
            state.apply(seq as u64 + 1, now, &event).unwrap();
        }
        let before = state.clone();
        let role = atm_org::parse_role("worker", "+++\nharness='pi'\nmanager='lead'\n+++\nFresh\n")
            .unwrap();
        let context =
            assemble_context(&state, state.agent(&id).unwrap(), Some(role), true).unwrap();
        assert_eq!(
            serde_json::to_value(context).unwrap(),
            serde_json::json!({
                "agent_id":"project/worker","manager":"project/lead",
                "role_file":"/project/.atm/roles/worker.md","instructions":"Fresh\n",
                "unread_messages":[{"message_id":"m4","kind":"note","from":"system","first_line":"First"}],
                "memory":{"path":"/project/.atm/memory/worker.md","exists":true},"open_tasks":[],
                "report_command":"atm msg send --to project/lead --kind report --body 'DONE: <result>'"
            })
        );
        assert_eq!(state, before);
        state
            .apply(
                5,
                now,
                &OrgEvent::ProjectRegistered {
                    project_id: id.project.clone(),
                    root: "relative".into(),
                },
            )
            .unwrap();
        assert!(matches!(
            assemble_context(&state, state.agent(&id).unwrap(), None, false),
            Err(AgentError::Refused(_))
        ));
    }

    // Break caught: caller timeout drops the worker's permit and permits an
    // unlimited replacement thread for the same stalled filesystem resource.
    #[tokio::test]
    async fn context_read_slot_survives_timeout() {
        let slot = Arc::new(Semaphore::new(1));
        let (entered, entry) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let first_slot = slot.clone();
        let first = tokio::spawn(async move {
            detached_read(
                first_slot,
                Path::new("/project/.atm/roles/worker.md"),
                move || {
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                    Ok(())
                },
            )
            .await
        });
        entry.await.unwrap();
        tokio::time::pause();
        let error = first.await.unwrap().unwrap_err();
        assert_eq!(error.0.code, "timeout");
        assert!(error.to_string().contains("reading agent context"));
        assert!(error.to_string().contains("/project/.atm/roles/worker.md"));
        assert!(!error.to_string().contains("admitted effects"));
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..20 {
            let count = calls.clone();
            let error = detached_read(
                slot.clone(),
                Path::new("/project/.atm/roles/worker.md"),
                move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.0.code, "timeout");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        tokio::time::resume();
        release.send(()).unwrap();
        // Permit release, not a delay, proves the original worker completed.
        drop(slot.clone().acquire_owned().await.unwrap());
        assert_eq!(
            detached_read(slot, Path::new("/project/.atm/roles/worker.md"), || Ok(7))
                .await
                .unwrap(),
            7
        );
    }

    // Break caught: dropping/cancelling the request releases the slot early.
    #[tokio::test]
    async fn context_read_slot_survives_cancellation() {
        let slot = Arc::new(Semaphore::new(1));
        let (entered, entry) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let copy = slot.clone();
        let first = tokio::spawn(async move {
            detached_read(
                copy,
                Path::new("/project/.atm/roles/worker.md"),
                move || {
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                    Ok(())
                },
            )
            .await
        });
        entry.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        tokio::time::pause();
        let error = detached_read(
            slot.clone(),
            Path::new("/project/.atm/roles/worker.md"),
            move || {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.0.code, "timeout");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        tokio::time::resume();
        release.send(()).unwrap();
        drop(slot.clone().acquire_owned().await.unwrap());
        detached_read(slot, Path::new("/project/.atm/roles/worker.md"), || Ok(()))
            .await
            .unwrap();
    }

    // Break caught: simultaneous fast reads refuse instead of waiting for the
    // same bounded resource; the reply arrives before capacity is released.
    #[tokio::test]
    async fn context_overlapping_reads_both_succeed() {
        let slot = Arc::new(Semaphore::new(1));
        let (entered, entry) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let copy = slot.clone();
        let first = tokio::spawn(async move {
            detached_read(
                copy,
                Path::new("/project/.atm/roles/worker.md"),
                move || {
                    entered.send(()).unwrap();
                    let _ = gate.recv();
                    Ok(11)
                },
            )
            .await
        });
        entry.await.unwrap();
        let mut second = Box::pin(detached_read(
            slot.clone(),
            Path::new("/project/.atm/roles/worker.md"),
            || Ok(22),
        ));
        std::future::poll_fn(|cx| {
            use std::{future::Future, task::Poll};
            assert!(
                second.as_mut().poll(cx).is_pending(),
                "overlapping reader must wait"
            );
            Poll::Ready(())
        })
        .await;
        release.send(()).unwrap();
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().unwrap(), 11);
        assert_eq!(second.unwrap(), 22);
        assert_eq!(
            slot.available_permits(),
            1,
            "reply must follow slot release"
        );
    }

    // Break caught: a cancelled waiter later spawns a detached reader anyway.
    #[tokio::test]
    async fn context_cancelled_waiter_never_starts_a_reader() {
        let slot = Arc::new(Semaphore::new(1));
        let permit = slot.clone().acquire_owned().await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut waiting = Box::pin(detached_read(
            slot.clone(),
            Path::new("/project/.atm/roles/worker.md"),
            move || {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        ));
        std::future::poll_fn(|cx| {
            use std::{future::Future, task::Poll};
            assert!(waiting.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(waiting);
        drop(permit);
        detached_read(slot, Path::new("/project/.atm/roles/worker.md"), || Ok(()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
