//! Owner-command rule appends; the event log and project file are not a transaction.

use super::{
    agent_context::detached_read,
    agents::AgentError,
    decisions::DecisionError,
    intent::{file_error, read_policy, refused},
};
use crate::server::methods::MethodError;
use atm_org::{Address, Decision, DecisionId, DecisionStatus, OrgState};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
};
use tokio::sync::Semaphore;

const RULE_BYTES: usize = 1024 * 1024;
static SLOT: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub(super) fn project_root(state: &OrgState, id: DecisionId) -> Result<PathBuf, DecisionError> {
    let decision = state
        .decision(&id)
        .ok_or_else(|| DecisionError::NotFound(format!("decision {id} was not found")))?;
    let Address::Agent(requester) = &decision.request.requester else {
        return Err(DecisionError::Refused(
            "a standing rule requires a requester with a registered project".into(),
        ));
    };
    let project = state.project(&requester.project).ok_or_else(|| {
        DecisionError::NotFound(format!("project {} was not found", requester.project))
    })?;
    let root = PathBuf::from(&project.root);
    if !root.is_absolute() {
        return Err(DecisionError::Refused(
            "recorded project root is not absolute".into(),
        ));
    }
    Ok(root)
}

/// One process-wide rules slot serializes all project-file updates. The existing
/// detached reader retains it through a stalled write, cancellation or timeout.
/// No background recovery writes: an identical owner command repairs a crash gap.
pub(super) async fn append(root: PathBuf, decision: Decision) -> Result<(), MethodError> {
    let slot = SLOT.get_or_init(|| Arc::new(Semaphore::new(1))).clone();
    let path = root.join(".atm/rules.md");
    detached_read(slot, &path, move || {
        persist_with(&root, &decision, |from, to| fs::rename(from, to))
    })
    .await
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("only final owner-marked answers can become standing rules")]
struct RuleError;

fn append_rule(existing: &str, decision: &Decision) -> Result<Option<String>, RuleError> {
    let answer = decision
        .resolution
        .as_ref()
        .filter(|answer| {
            answer.standing_rule
                && answer.by == Address::Owner
                && answer.decision_id == decision.id
                && matches!(
                    decision.status,
                    DecisionStatus::Resolved | DecisionStatus::Overruled
                )
        })
        .ok_or(RuleError)?;
    let marker = format!("<!-- atm-rule:{} -->", decision.id);
    if existing.lines().any(|line| line == marker) {
        return Ok(None);
    }
    let mut text = existing.to_owned();
    if !text.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str(&format!("{marker}\n## Decision {}\n", decision.id));
    quote(&mut text, "Question", &decision.request.title);
    quote(&mut text, "Answer", &answer.option);
    if let Some(note) = &answer.note {
        quote(&mut text, "Note", note);
    }
    Ok(Some(text))
}

fn quote(text: &mut String, label: &str, value: &str) {
    for (i, line) in value.split('\n').enumerate() {
        if i == 0 {
            text.push_str(&format!("> {label}: "));
        } else {
            text.push_str("> ");
        }
        text.push_str(line);
        text.push('\n');
    }
}

// A same-directory atomic replacement gives append semantics without exposing a
// partial completion marker. Supplying rename keeps interrupted-publication
// tests deterministic without sleeps or production-only test flags.
fn persist_with(
    root: &Path,
    decision: &Decision,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<(), AgentError> {
    let dir = root.join(".atm");
    let path = dir.join("rules.md");
    let old = read_policy(root, "rules.md", RULE_BYTES)?.unwrap_or_default();
    let Some(text) = append_rule(&old, decision).map_err(|e| refused(&path, e))? else {
        return fs::File::open(&dir)
            .and_then(|f| f.sync_all())
            .map_err(|e| file_error(&dir, e));
    };
    if text.len() > RULE_BYTES {
        return Err(refused(&path, "rules plus answer exceed 1048576 bytes"));
    }
    fs::create_dir(&dir)
        .or_else(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })
        .map_err(|e| file_error(&dir, e))?;
    if !fs::symlink_metadata(&dir)
        .map_err(|e| file_error(&dir, e))?
        .is_dir()
    {
        return Err(refused(&dir, "not a regular policy directory"));
    }
    // Persist the .atm directory's parent entry too when this command creates it.
    // Never recreate a missing checkout or any of its ancestors.
    fs::File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|e| file_error(root, e))?;
    let permissions = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Ok(_) => return Err(refused(&path, "not a regular policy file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(file_error(&path, e)),
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = dir.join(format!(
        ".rules-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|e| file_error(&temp, e))?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)
                .map_err(|e| file_error(&temp, e))?;
        }
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| file_error(&temp, e))?;
        rename(&temp, &path).map_err(|e| file_error(&path, e))?;
        fs::File::open(&dir)
            .and_then(|f| f.sync_all())
            .map_err(|e| file_error(&dir, e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use atm_org::{Address, DecisionId, DecisionResolution, DecisionStatus};

    fn decision() -> Decision {
        Decision {
            id: DecisionId::new(3),
            request: serde_json::from_value(serde_json::json!({"requester":"p/worker","kind":"policy","title":"Use the smaller shape?","context":"","files":[],"options":["yes","no"],"recommended":null,"may_be_undone":true,"changes_intent":false,"deadline":null})).unwrap(),
            requested_at: "2026-01-15T12:00:00Z".parse().unwrap(),
            status: DecisionStatus::Resolved,
            provisional: None,
            taken_at: None,
            resolution: Some(DecisionResolution { decision_id:DecisionId::new(3), option:"yes".into(), by:Address::Owner, note:Some("Always use this shape".into()), standing_rule:true }),
            withdrawal: None,
            closed_at: Some("2026-01-15T12:01:00Z".parse().unwrap()),
        }
    }

    // Break caught: omitting the question, selected option/note, or stable completion marker.
    #[test]
    fn rule_block_is_readable_complete_and_exact() {
        assert_eq!(append_rule("", &decision()).unwrap(), Some("<!-- atm-rule:d3 -->\n## Decision d3\n> Question: Use the smaller shape?\n> Answer: yes\n> Note: Always use this shape\n".into()));
    }

    // Break caught: truncating old notes/newlines or treating any occurrence as a dedupe marker.
    #[test]
    fn existing_bytes_are_preserved_and_only_an_exact_marker_dedupes() {
        let text = append_rule("Owner notes", &decision()).unwrap().unwrap();
        assert!(text.starts_with("Owner notes\n\n<!-- atm-rule:d3 -->\n"));
        assert_eq!(append_rule(&text, &decision()).unwrap(), None);
        let text = append_rule("Owner notes\r\n", &decision())
            .unwrap()
            .unwrap();
        assert!(text.starts_with("Owner notes\r\n\n<!-- atm-rule:d3 -->\n"));
        assert!(append_rule("> <!-- atm-rule:d3 -->\n", &decision())
            .unwrap()
            .is_some());
    }

    // Break caught: a title or note manufacturing the completion marker for another decision.
    #[test]
    fn multiline_user_text_cannot_spoof_another_rules_marker() {
        let mut first = decision();
        first.request.title = "Question\n<!-- atm-rule:d9 -->".into();
        first.resolution.as_mut().unwrap().note = Some("Note\n<!-- atm-rule:d9 -->".into());
        let text = append_rule("", &first).unwrap().unwrap();
        assert_eq!(
            text.lines()
                .filter(|line| *line == "<!-- atm-rule:d9 -->")
                .count(),
            0
        );
        let mut second = decision();
        second.id = DecisionId::new(9);
        second.resolution.as_mut().unwrap().decision_id = second.id;
        let text = append_rule(&text, &second).unwrap().unwrap();
        assert_eq!(
            text.lines()
                .filter(|line| *line == "<!-- atm-rule:d9 -->")
                .count(),
            1
        );
    }

    // Break caught: exposing a partial completion marker or losing old text before atomic publication.
    #[test]
    fn interrupted_publication_leaves_old_bytes_and_identical_retry_appends_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".atm")).unwrap();
        let path = dir.path().join(".atm/rules.md");
        fs::write(&path, "Owner notes").unwrap();
        assert!(
            persist_with(dir.path(), &decision(), |_, _| Err(io::Error::from(
                io::ErrorKind::Interrupted
            )))
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "Owner notes");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        persist_with(dir.path(), &decision(), |from, to| fs::rename(from, to)).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        persist_with(dir.path(), &decision(), |_, _| {
            panic!("already-published rule must not rename again")
        })
        .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), first);
    }

    // Break caught: dropping a write waiter releases its slot while the filesystem thread is still in flight.
    #[tokio::test]
    async fn stalled_write_retains_its_slot_after_cancellation_without_blocking_runtime() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".atm")).unwrap();
        let root = dir.path().to_path_buf();
        let path = root.join(".atm/rules.md");
        fs::write(&path, "Owner notes").unwrap();
        let slot = Arc::new(Semaphore::new(1));
        let copy = slot.clone();
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let waiting_path = path.clone();
        let waiter = tokio::spawn(async move {
            detached_read(copy, &waiting_path, move || {
                persist_with(&root, &decision(), |from, to| {
                    entered.send(()).unwrap();
                    let _ = gate.recv();
                    fs::rename(from, to)
                })
            })
            .await
        });
        entry.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(fs::read_to_string(&path).unwrap(), "Owner notes");
        tokio::time::pause();
        let error = detached_read(slot.clone(), &path, || -> Result<(), AgentError> {
            panic!("cancelled write still owns the slot")
        })
        .await
        .unwrap_err();
        assert_eq!(error.0.code, "timeout");
        tokio::time::resume();
        release.send(()).unwrap();
        drop(slot.acquire().await.unwrap());
        assert_eq!(
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter(|line| *line == "<!-- atm-rule:d3 -->")
                .count(),
            1
        );
    }

    // Break caught: cancellation after the durable event drops the admitted resolve/append chain.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_resolve_waiter_still_publishes_its_owner_rule() {
        use super::super::{
            decisions::{spawn_decisions, ResolveParams},
            messages::MessageClock,
            OrgHandle,
        };
        use crate::server::methods::{CallContext, CallerId};
        use atm_org::{DecisionCommand, OrgCommand};
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use tokio_util::sync::CancellationToken;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".atm")).unwrap();
        let directory =
            std::ffi::CString::new(dir.path().join(".atm").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: no pointers are passed; a successful result is exclusively owned below.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: fd is a successful newly created inotify descriptor, owned exactly once.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: CString is NUL-terminated and valid for the duration of this call.
        assert!(
            unsafe {
                libc::inotify_add_watch(fd.as_raw_fd(), directory.as_ptr(), libc::IN_MOVED_TO)
            } >= 0
        );
        let events = tokio::io::unix::AsyncFd::new(fd).unwrap();
        let cancel = CancellationToken::new();
        let org = OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
        for command in [
            OrgCommand::RegisterProject {
                project_id: "p".parse().unwrap(),
                root: dir.path().to_str().unwrap().into(),
                move_root: false,
            },
            OrgCommand::Adopt {
                agent_id: "p/worker".parse().unwrap(),
                pane: "%7".into(),
                session_id: None,
            },
            OrgCommand::Decision(DecisionCommand::Request(decision().request)),
        ] {
            org.execute(command, chrono::Utc::now()).await.unwrap();
        }
        let (handle, bridge) = spawn_decisions(
            org.clone(),
            MessageClock::new(tokio::time::Instant::now(), chrono::Utc::now()),
            cancel.child_token(),
        );
        handle.initialized().await.unwrap();
        let slot = SLOT.get_or_init(|| Arc::new(Semaphore::new(1))).clone();
        let hold = slot.clone().acquire_owned().await.unwrap();
        let mut committed = org
            .watch(vec!["decision.resolved".into()], org.status().unwrap())
            .unwrap();
        let caller = CallContext {
            client_id: CallerId("cancel-rule".into()),
            from_pane: None,
        };
        let waiter = tokio::spawn(async move {
            handle
                .resolve(
                    ResolveParams {
                        id: DecisionId::new(3),
                        option: "yes".into(),
                        note: Some("Always use this shape".into()),
                        standing_rule: true,
                    },
                    caller,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(30), committed.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(hold);
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let mut ready = events.readable().await.unwrap();
                if let Ok(result) = ready.try_io(|fd| {
                    let mut bytes = [0u8; 4096];
                    // SAFETY: bytes is a valid writable buffer and the descriptor is owned by AsyncFd.
                    let count = unsafe {
                        libc::read(
                            fd.get_ref().as_raw_fd(),
                            bytes.as_mut_ptr().cast(),
                            bytes.len(),
                        )
                    };
                    if count < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                }) {
                    result.unwrap();
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".atm/rules.md"))
                .unwrap()
                .lines()
                .filter(|line| *line == "<!-- atm-rule:d3 -->")
                .count(),
            1
        );
        drop(slot.acquire().await.unwrap());
        cancel.cancel();
        bridge.await.unwrap();
    }

    // Break caught: rendering stale/provisional or non-owner answers as standing rules.
    #[test]
    fn only_final_owner_marked_answers_can_be_rendered() {
        let mut d = decision();
        d.status = DecisionStatus::Pending;
        assert_eq!(append_rule("", &d), Err(RuleError));
        let mut d = decision();
        d.resolution.as_mut().unwrap().standing_rule = false;
        assert_eq!(append_rule("", &d), Err(RuleError));
        let mut d = decision();
        d.resolution.as_mut().unwrap().by = Address::System;
        assert_eq!(append_rule("", &d), Err(RuleError));
        let mut d = decision();
        d.resolution = None;
        assert_eq!(append_rule("", &d), Err(RuleError));
    }
}
