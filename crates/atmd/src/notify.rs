//! Runs the user's `[notify]` command when a session starts needing input.
//!
//! The command comes from atm's `config.toml` (`[notify] command = [..]`),
//! which the `atmd` binary reads once at start, so a change needs a
//! restart. The notifier only listens to the registry's broadcast: it
//! fires when an `Updated` row's status turns into
//! [`SessionStatus::AttentionNeeded`](atm_core::SessionStatus) and the
//! row's last seen status was something else. Each run gets the message
//! as one last argument, no shell, atmd's environment, closed stdin and
//! stdout, and [`NOTIFY_TIMEOUT`].
//!
//! Claude Code 2.1.310 sends `PermissionRequest` and then `Notification`
//! (`permission_prompt`) for one dialog, both needs input, so one dialog
//! notifies once; `idle_prompt` maps to idle and never notifies (the
//! root `tests/atmd_notify.rs` replays that capture).
//!
//! A subagent's dialog turns two rows: seen with a real Claude Code
//! 2.1.317 TUI on 2026-11-03 (a fake Messages API, no model call), its
//! `PermissionRequest` carries the subagent's `agent_id`, and the
//! `Notification` about 6 s later, sent only if the dialog is still open,
//! carries none, so it lands on the lead's row. A lead with a subagent
//! already needing input therefore stays quiet, once per subagent stretch:
//! a subagent stopped mid-dialog keeps needing input (#129) and must not
//! mute the lead's later dialogs.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use atm_core::{SessionId, SessionView};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::registry::SessionEvent;

/// How long a notify command may run before atmd kills it.
pub const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a failed command's stderr atmd keeps for its log.
pub const STDERR_LIMIT: usize = 4096;

/// A `[notify]` config or command that failed.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// atm's `config.toml` is not valid, or its `[notify]` is not.
    #[error("invalid config: {0}")]
    Config(#[from] toml::de::Error),
    /// The command could not start, e.g. it was not found.
    #[error("failed to start {program:?}: {source}")]
    Start {
        program: String,
        source: std::io::Error,
    },
    /// Waiting for the command failed.
    #[error("failed to wait for {program:?}: {source}")]
    Wait {
        program: String,
        source: std::io::Error,
    },
    /// The command outlived its timeout and was killed.
    #[error("{program:?} did not finish within {timeout:?}; killed it")]
    Timeout { program: String, timeout: Duration },
    /// The command exited unsuccessfully.
    #[error("{program:?} failed ({status}): {stderr}")]
    Failed {
        program: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
}

/// The parts of atm's `config.toml` that atmd reads.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    notify: NotifyConfig,
}

/// `[notify]` in atm's `config.toml`. An unknown key is an error, so a
/// typo doesn't turn notifications off silently.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyConfig {
    /// The program and its arguments. atmd appends the message as one
    /// last argument and runs it without a shell.
    #[serde(default)]
    command: Vec<String>,
}

/// The `[notify]` command in the text of atm's `config.toml`. `None`, so
/// notifications are off, when there is no section or `command` is empty.
pub fn parse_notify_command(text: &str) -> Result<Option<Vec<String>>, NotifyError> {
    let file: ConfigFile = toml::from_str(text)?;
    Ok(Some(file.notify.command).filter(|command| !command.is_empty()))
}

/// The configured command followed by `message` as its last argument.
pub fn notify_argv(command: &[String], message: &str) -> Vec<String> {
    [command, &[message.to_string()]].concat()
}

/// Says which agent needs input and why: its project label (as in
/// `atm list`), its pane or short id, its harness, and the activity.
pub fn notification_message(view: &SessionView) -> String {
    let dir = view
        .project_root
        .as_ref()
        .or(view.working_directory.as_ref());
    let project = dir
        .and_then(|dir| dir.rsplit('/').find(|part| !part.is_empty()))
        .unwrap_or("-");
    let label = match &view.worktree_branch {
        Some(branch) => format!("{project}/{branch}"),
        None => project.to_string(),
    };
    let target = view.tmux_pane.as_deref().unwrap_or(&view.id_short);
    let who = if view.parent_session_id.is_some() {
        format!("{} subagent", view.harness)
    } else {
        view.harness.clone()
    };
    let why = view
        .activity_detail
        .as_ref()
        .map(|detail| format!(" ({detail})"))
        .unwrap_or_default();
    format!("{label} {target}: {who} needs input{why}")
}

/// Remembers which sessions need input, to notify only on the change.
#[derive(Debug, Default)]
pub struct AttentionTracker {
    /// Each session that needs input, and whether it has already muted its
    /// lead's ping during this stretch.
    in_attention: HashMap<SessionId, bool>,
}

impl AttentionTracker {
    /// The message to send when `event` turns a session into needing input.
    ///
    /// `Registered` carries no status, so it only resets the session to
    /// "not needing input"; the next `Updated` decides. A session whose
    /// subagent already needs input stays quiet, because Claude reports a
    /// subagent's dialog on the lead's row too, but each subagent mutes it
    /// only once per stretch: a stopped one can keep needing input (#129).
    pub fn observe(&mut self, event: &SessionEvent) -> Option<String> {
        match event {
            SessionEvent::Registered { session_id, .. }
            | SessionEvent::Removed { session_id, .. } => {
                self.in_attention.remove(session_id);
                None
            }
            SessionEvent::Updated { session } if session.status.needs_attention() => {
                if self.in_attention.contains_key(&session.id) {
                    return None;
                }
                self.in_attention.insert(session.id.clone(), false);
                let muting_child = (session.child_session_ids.iter())
                    .find(|child| self.in_attention.get(*child) == Some(&false));
                match muting_child.and_then(|child| self.in_attention.get_mut(child)) {
                    Some(muted) => {
                        *muted = true;
                        None
                    }
                    None => Some(notification_message(session)),
                }
            }
            SessionEvent::Updated { session } => {
                self.in_attention.remove(&session.id);
                None
            }
        }
    }
}

/// Runs `argv` without a shell, with stdin and stdout closed, and waits up
/// to `timeout`. Its stderr goes into the error. On timeout the command is
/// killed; a process it started itself keeps running.
pub async fn run_notify(argv: &[String], timeout: Duration) -> Result<(), NotifyError> {
    // An empty argv fails to start, like a program that isn't there.
    let program = argv.first().cloned().unwrap_or_default();
    let args = argv.get(1..).unwrap_or_default();
    let mut child = Command::new(&program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| NotifyError::Start {
            program: program.clone(),
            source,
        })?;
    let mut stderr = Vec::new();
    let pipe = child.stderr.take();
    let run = async {
        if let Some(pipe) = pipe {
            // Past the limit the pipe closes, so a chatty command gets
            // EPIPE instead of growing atmd's memory.
            pipe.take(STDERR_LIMIT as u64)
                .read_to_end(&mut stderr)
                .await?;
        }
        child.wait().await
    };
    let status = match tokio::time::timeout(timeout, run).await {
        Ok(status) => status.map_err(|source| NotifyError::Wait {
            program: program.clone(),
            source,
        })?,
        // Dropping `run` let go of the child; `kill_on_drop` kills it when
        // this function returns.
        Err(_) => return Err(NotifyError::Timeout { program, timeout }),
    };
    if status.success() {
        return Ok(());
    }
    Err(NotifyError::Failed {
        program,
        status,
        stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
    })
}

/// Runs `command` with a message each time a session starts needing
/// input, until `cancel` fires or the registry closes. Each command runs in
/// its own task, so a slow one never holds up the next event.
pub fn spawn_notifier(
    mut events: broadcast::Receiver<SessionEvent>,
    command: Vec<String>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker = AttentionTracker::default();
        loop {
            let event = tokio::select! {
                () = cancel.cancelled() => break,
                event = events.recv() => event,
            };
            match event {
                Ok(event) => {
                    if let Some(message) = tracker.observe(&event) {
                        let argv = notify_argv(&command, &message);
                        tokio::spawn(async move {
                            match run_notify(&argv, NOTIFY_TIMEOUT).await {
                                Ok(()) => info!(message = %message, "Notify command ran"),
                                Err(e) => {
                                    warn!(error = %e, message = %message, "Notify command failed")
                                }
                            }
                        });
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "Notifier fell behind and missed session events");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use atm_core::{AgentType, SessionStatus};

    use super::*;
    use crate::registry::RemovalReason;

    #[test]
    fn parses_the_notify_command() {
        let text =
            "[harness]\ndefault = 'pi'\n\n[notify]\ncommand = ['notify-send', 'atm', '--']\n";
        assert_eq!(
            parse_notify_command(text).unwrap(),
            Some(vec!["notify-send".into(), "atm".into(), "--".into()])
        );
    }

    #[test]
    fn no_section_or_an_empty_command_turns_notify_off() {
        for text in [
            "",
            "[harness]\ndefault = 'claude'\n",
            "[notify]\n",
            "[notify]\ncommand = []\n",
        ] {
            assert_eq!(parse_notify_command(text).unwrap(), None, "{text:?}");
        }
    }

    #[test]
    fn an_invalid_config_is_an_error() {
        for text in [
            "[notify]\ncommand = 'notify-send atm'\n",
            "[notify]\ncommand = [1]\n",
            "[notify\n",
            // A typo inside [notify] is an error, not notifications off.
            "[notify]\ncmd = ['notify-send', 'atm', '--']\n",
        ] {
            assert!(parse_notify_command(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn the_message_is_one_last_argument() {
        let command = ["notify-send", "atm", "--"].map(String::from);
        for message in [
            "two words",
            "it's \"quoted\"",
            "$(touch pwned) `id` $HOME *",
            "line one\nline two",
            "-x starts with a dash",
            "",
        ] {
            assert_eq!(
                notify_argv(&command, message),
                ["notify-send", "atm", "--", message]
            );
        }
    }

    fn view(id: &str, status: SessionStatus) -> SessionView {
        SessionView {
            id: SessionId::new(id),
            id_short: id.chars().take(8).collect(),
            harness: "claude".into(),
            status,
            ..SessionView::default()
        }
    }

    fn updated(id: &str, status: SessionStatus) -> SessionEvent {
        SessionEvent::Updated {
            session: Box::new(view(id, status)),
        }
    }

    fn registered(id: &str) -> SessionEvent {
        SessionEvent::Registered {
            session_id: SessionId::new(id),
            agent_type: AgentType::GeneralPurpose,
        }
    }

    fn removed(id: &str) -> SessionEvent {
        SessionEvent::Removed {
            session_id: SessionId::new(id),
            reason: RemovalReason::SessionEnded,
        }
    }

    /// Feeds `events` to a fresh tracker and returns, per event, whether
    /// it fired.
    fn fired(events: &[SessionEvent]) -> Vec<bool> {
        let mut tracker = AttentionTracker::default();
        events
            .iter()
            .map(|event| tracker.observe(event).is_some())
            .collect()
    }

    use SessionStatus::{AttentionNeeded, Idle, Working};

    #[test]
    fn fires_when_a_session_starts_needing_input() {
        assert_eq!(
            fired(&[
                registered("a"),
                updated("a", Idle),
                updated("a", AttentionNeeded)
            ]),
            [false, false, true]
        );
        assert_eq!(
            fired(&[updated("a", Working), updated("a", AttentionNeeded)]),
            [false, true]
        );
    }

    /// Claude sends `PermissionRequest` and then `Notification`
    /// (`permission_prompt`) for one dialog, and both map to needs input.
    #[test]
    fn stays_quiet_while_a_session_keeps_needing_input() {
        assert_eq!(
            fired(&[
                updated("a", Working),
                updated("a", AttentionNeeded),
                updated("a", AttentionNeeded),
            ]),
            [false, true, false]
        );
    }

    #[test]
    fn fires_again_for_the_next_dialog() {
        assert_eq!(
            fired(&[
                updated("a", AttentionNeeded),
                updated("a", Working),
                updated("a", AttentionNeeded),
            ]),
            [true, false, true]
        );
    }

    /// `Registered` carries no status, so it only resets the baseline.
    /// Discovery's rows start idle, so startup sends nothing.
    #[test]
    fn registering_never_fires() {
        assert_eq!(
            fired(&[registered("a"), updated("a", Idle), registered("b")]),
            [false, false, false]
        );
    }

    #[test]
    fn a_session_that_left_and_came_back_starts_fresh() {
        assert_eq!(
            fired(&[
                updated("a", AttentionNeeded),
                removed("a"),
                registered("a"),
                updated("a", AttentionNeeded),
            ]),
            [true, false, false, true]
        );
    }

    /// Seen with a real Claude Code 2.1.317 TUI on 2026-11-03: a
    /// subagent's permission dialog sends `PermissionRequest` with its
    /// `agent_id` (the subagent's row needs input), and about 6 s later,
    /// if the dialog is still open, a `Notification` (`permission_prompt`)
    /// without one (the lead's row needs input too). One dialog, one
    /// notification: the subagent's, which names the tool.
    #[test]
    fn a_lead_stays_quiet_while_its_subagent_needs_input() {
        let mut lead = view("lead", Working);
        lead.child_session_ids = vec![SessionId::new("sub")];
        let mut sub = view("sub", AttentionNeeded);
        sub.parent_session_id = Some(SessionId::new("lead"));
        let lead_now = |status| SessionEvent::Updated {
            session: Box::new(SessionView {
                status,
                ..lead.clone()
            }),
        };
        let sub_now = |status| SessionEvent::Updated {
            session: Box::new(SessionView {
                status,
                ..sub.clone()
            }),
        };
        assert_eq!(
            fired(&[
                lead_now(Working),
                sub_now(AttentionNeeded),
                lead_now(AttentionNeeded),
                // The user answers; the subagent ends.
                sub_now(Working),
                removed("sub"),
                updated("lead", Idle),
                // The lead's own dialog, with no subagent waiting.
                updated("lead", AttentionNeeded),
            ]),
            [false, true, false, false, false, false, true]
        );
    }

    /// #129, captured with Claude Code 2.1.312 on 2026-10-29: a background
    /// agent stopped during its dialog sends no `SubagentStop`, so its row
    /// keeps needing input. It mutes the lead's ping for its own dialog,
    /// but not the lead's later dialogs.
    #[test]
    fn a_stale_subagent_mutes_its_lead_only_once() {
        let mut lead = view("lead", Working);
        lead.child_session_ids = vec![SessionId::new("sub")];
        let mut sub = view("sub", AttentionNeeded);
        sub.parent_session_id = Some(SessionId::new("lead"));
        let lead_now = |status| SessionEvent::Updated {
            session: Box::new(SessionView {
                status,
                ..lead.clone()
            }),
        };
        let sub_now = |status| SessionEvent::Updated {
            session: Box::new(SessionView {
                status,
                ..sub.clone()
            }),
        };
        assert_eq!(
            fired(&[
                sub_now(AttentionNeeded),
                lead_now(AttentionNeeded),
                // The subagent is stopped; its row stays in needs input.
                lead_now(Working),
                lead_now(Idle),
                lead_now(AttentionNeeded),
                lead_now(Working),
                lead_now(AttentionNeeded),
            ]),
            [true, false, false, false, true, false, true]
        );
        // Its next dialog, after leaving needs input, mutes the lead again.
        assert_eq!(
            fired(&[
                sub_now(AttentionNeeded),
                lead_now(AttentionNeeded),
                lead_now(Working),
                sub_now(Working),
                sub_now(AttentionNeeded),
                lead_now(AttentionNeeded),
            ]),
            [true, false, false, false, true, false]
        );
    }

    #[test]
    fn sessions_are_tracked_apart() {
        assert_eq!(
            fired(&[
                updated("a", AttentionNeeded),
                updated("b", AttentionNeeded),
                updated("a", AttentionNeeded),
            ]),
            [true, true, false]
        );
    }

    #[test]
    fn the_message_names_the_agent_and_why() {
        let mut lead = view("3f2a9c1d-lead", AttentionNeeded);
        lead.project_root = Some("/home/u/git/agent-tmux-manager".into());
        lead.worktree_branch = Some("feat/atm-notify".into());
        lead.tmux_pane = Some("%168".into());
        lead.activity_detail = Some("Permission".into());
        assert_eq!(
            notification_message(&lead),
            "agent-tmux-manager/feat/atm-notify %168: claude needs input (Permission)"
        );

        let mut child = lead.clone();
        child.parent_session_id = Some(SessionId::new("3f2a9c1d-lead"));
        assert_eq!(
            notification_message(&child),
            "agent-tmux-manager/feat/atm-notify %168: claude subagent needs input (Permission)"
        );

        // No git context or pane yet: the working directory and the short
        // id stand in, so the user can still `atm reply` it.
        let mut bare = view("9b8c7d6e5f", AttentionNeeded);
        bare.working_directory = Some("~/scratch/demo".into());
        assert_eq!(
            notification_message(&bare),
            "demo 9b8c7d6e: claude needs input"
        );

        // Nothing at all: `-`, as `atm list` shows a missing project.
        assert_eq!(
            notification_message(&view("9b8c7d6e5f", AttentionNeeded)),
            "- 9b8c7d6e: claude needs input"
        );
    }

    #[tokio::test]
    async fn run_notify_reports_a_command_that_fails() {
        let argv = ["sh", "-c", "echo 'webhook said no' >&2; exit 3"].map(String::from);
        let err = run_notify(&argv, Duration::from_secs(5))
            .await
            .expect_err("exit 3 is a failure");
        let message = err.to_string();
        assert!(message.contains("exit status: 3"), "{message}");
        assert!(message.contains("webhook said no"), "{message}");
    }

    /// A chatty command can't grow atmd's memory or its log line: atmd
    /// keeps at most `STDERR_LIMIT` bytes of its stderr.
    #[tokio::test]
    async fn run_notify_keeps_only_the_start_of_a_long_stderr() {
        let argv = [
            "sh",
            "-c",
            "head -c 1000000 /dev/zero | tr '\\0' x >&2; exit 1",
        ]
        .map(String::from);
        let err = run_notify(&argv, Duration::from_secs(10))
            .await
            .expect_err("a chatty failure is still a failure");
        let NotifyError::Failed { stderr, .. } = &err else {
            panic!("expected Failed, got {err}");
        };
        assert!(
            !stderr.is_empty() && stderr.len() <= STDERR_LIMIT,
            "{}",
            stderr.len()
        );
    }

    #[tokio::test]
    async fn run_notify_names_a_command_that_cannot_start() {
        let argv = ["/nonexistent/atm-notifier", "hi"].map(String::from);
        let err = run_notify(&argv, Duration::from_secs(5))
            .await
            .expect_err("a missing program cannot start");
        assert!(matches!(err, NotifyError::Start { .. }), "{err}");
        assert!(err.to_string().contains("atm-notifier"), "{err}");
    }

    #[tokio::test]
    async fn run_notify_succeeds_on_exit_zero() {
        let argv = ["sh", "-c", "exit 0"].map(String::from);
        run_notify(&argv, Duration::from_secs(5)).await.unwrap();
    }

    /// Live (not zombie) children of this process whose command line
    /// contains `marker`. A zombie's cmdline is empty, so it never matches.
    #[cfg(target_os = "linux")]
    fn live_children_with(marker: &str) -> Vec<u32> {
        let me = std::process::id().to_string();
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| {
                let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                // `pid (comm) state ppid ...`; comm may hold spaces or `)`.
                let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?;
                (ppid == me && String::from_utf8_lossy(&cmdline).contains(marker)).then_some(pid)
            })
            .collect()
    }

    /// The timeout kills the command instead of leaving it running. The
    /// marker is an odd sleep length, so only this test's child matches.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn run_notify_kills_a_command_that_outlives_the_timeout() {
        let marker = "5.031337";
        let argv = ["sh", "-c", "exec sleep \"$1\"", "sh", marker].map(String::from);

        let started = Instant::now();
        let err = run_notify(&argv, Duration::from_millis(200))
            .await
            .expect_err("a 5 s command must time out after 200 ms");
        assert!(started.elapsed() < Duration::from_secs(3), "{err}");
        assert!(matches!(err, NotifyError::Timeout { .. }), "{err}");

        // SIGKILL is asynchronous: give the kernel a moment.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !live_children_with(marker).is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(live_children_with(marker), Vec::<u32>::new());
    }
}
