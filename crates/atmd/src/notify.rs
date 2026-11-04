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
//! With `delay_secs`, a notification waits that long and goes out only if
//! the same stretch of needing input is still open, so a dialog answered
//! in time sends nothing. Claude Code 2.1.317 sends no hook when a dialog
//! is approved (seen 2026-11-04, sealed real TUI): the row reads needs
//! input until the tool's `PostToolUse`, so a tool that outlasts the delay
//! still notifies. The fix belongs where the event is mapped, not here.
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
use tokio::sync::mpsc;
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
    /// The command could not start (e.g. it was not found) or be waited on.
    #[error("{program:?}: {source}")]
    Io {
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
pub struct NotifyConfig {
    /// The program and its arguments. atmd appends the message as one
    /// last argument and runs it without a shell.
    #[serde(default)]
    pub command: Vec<String>,
    /// Seconds to wait first: a session that stops needing input by then
    /// sends nothing. 0, the default, notifies at once.
    #[serde(default)]
    pub delay_secs: u64,
}

/// The `[notify]` section in the text of atm's `config.toml`. `None`, so
/// notifications are off, when there is no section or `command` is empty.
pub fn parse_notify_command(text: &str) -> Result<Option<NotifyConfig>, NotifyError> {
    let file: ConfigFile = toml::from_str(text)?;
    Ok(Some(file.notify).filter(|notify| !notify.command.is_empty()))
}

/// The configured command followed by `message` as its last argument.
pub fn notify_argv(command: &[String], message: &str) -> Vec<String> {
    [command, &[message.to_string()]].concat()
}

/// Says which agent needs input and why: its project dir (and branch),
/// its pane or short id, its harness, and the activity.
pub fn notification_message(view: &SessionView) -> String {
    let dir = view
        .project_root
        .as_ref()
        .or(view.working_directory.as_ref());
    let project = dir.and_then(|d| d.rsplit('/').find(|p| !p.is_empty()));
    let branch = view.worktree_branch.as_ref().map(|b| format!("/{b}"));
    let target = view.tmux_pane.as_ref().unwrap_or(&view.id_short);
    let subagent = view.parent_session_id.as_ref().map_or("", |_| " subagent");
    let why = view.activity_detail.as_ref().map(|d| format!(" ({d})"));
    format!(
        "{}{} {target}: {}{subagent} needs input{}",
        project.unwrap_or("-"),
        branch.unwrap_or_default(),
        view.harness,
        why.unwrap_or_default()
    )
}

/// A notification for a session that started needing input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The session that needs input.
    pub session_id: SessionId,
    /// Which stretch of needing input it is, for [`AttentionTracker::is_open`].
    pub stretch: u64,
    /// The message, built when the stretch began.
    pub message: String,
}

/// One unbroken stretch of a session needing input.
#[derive(Debug, Clone, Copy)]
struct Stretch {
    id: u64,
    /// Whether this session has already muted its lead's ping.
    muted_lead: bool,
}

/// Remembers which sessions need input, to notify only on the change.
#[derive(Debug, Default)]
pub struct AttentionTracker {
    in_attention: HashMap<SessionId, Stretch>,
    last_stretch: u64,
}

impl AttentionTracker {
    /// The message to send when `event` turns a session into needing input.
    ///
    /// `Registered` carries no status, so it only resets the session to
    /// "not needing input"; the next `Updated` decides. A session whose
    /// subagent already needs input stays quiet, because Claude reports a
    /// subagent's dialog on the lead's row too, but each subagent mutes it
    /// only once per stretch: a stopped one can keep needing input (#129).
    pub fn observe(&mut self, event: &SessionEvent) -> Option<Pending> {
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
                self.last_stretch = self.last_stretch.wrapping_add(1);
                let stretch = Stretch {
                    id: self.last_stretch,
                    muted_lead: false,
                };
                self.in_attention.insert(session.id.clone(), stretch);
                let muting_child = (session.child_session_ids.iter())
                    .find(|child| self.in_attention.get(*child).is_some_and(|s| !s.muted_lead));
                match muting_child.and_then(|child| self.in_attention.get_mut(child)) {
                    Some(child) => {
                        child.muted_lead = true;
                        None
                    }
                    None => Some(Pending {
                        session_id: session.id.clone(),
                        stretch: stretch.id,
                        message: notification_message(session),
                    }),
                }
            }
            SessionEvent::Updated { session } => {
                self.in_attention.remove(&session.id);
                None
            }
        }
    }

    /// Whether `session_id` still needs input in the same `stretch`.
    pub fn is_open(&self, session_id: &SessionId, stretch: u64) -> bool {
        self.in_attention
            .get(session_id)
            .is_some_and(|open| open.id == stretch)
    }
}

/// Runs `argv` without a shell, with stdin and stdout closed, and waits up
/// to `timeout`. Its stderr goes into the error. On timeout the command is
/// killed; a process it started itself keeps running.
pub async fn run_notify(argv: &[String], timeout: Duration) -> Result<(), NotifyError> {
    // An empty argv fails to start, like a program that isn't there.
    let program = argv.first().cloned().unwrap_or_default();
    let args = argv.get(1..).unwrap_or_default();
    let io = |source| NotifyError::Io {
        program: program.clone(),
        source,
    };
    let mut child = Command::new(&program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(io)?;
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
        Ok(status) => status.map_err(io)?,
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
///
/// With a non-zero `delay`, each notification waits in its own timer task,
/// which then reports back here; it runs only if the session still needs
/// input in the same stretch. Only this loop touches the tracker.
pub fn spawn_notifier(
    mut events: broadcast::Receiver<SessionEvent>,
    command: Vec<String>,
    delay: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker = AttentionTracker::default();
        let (due_tx, mut due) = mpsc::unbounded_channel::<Pending>();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                Some(pending) = due.recv() => {
                    if tracker.is_open(&pending.session_id, pending.stretch) {
                        spawn_run(&command, pending.message);
                    }
                }
                event = events.recv() => match event {
                    Ok(event) => match tracker.observe(&event) {
                        Some(pending) if delay.is_zero() => spawn_run(&command, pending.message),
                        Some(pending) => {
                            let due_tx = due_tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(delay).await;
                                // A send error means the notifier has stopped.
                                let _ = due_tx.send(pending);
                            });
                        }
                        None => {}
                    },
                    Err(RecvError::Lagged(skipped)) => warn!(skipped, "Notifier missed events"),
                    Err(RecvError::Closed) => break,
                },
            }
        }
    })
}

/// Runs `command` with `message` in its own task and logs the outcome.
fn spawn_run(command: &[String], message: String) {
    let argv = notify_argv(command, &message);
    tokio::spawn(async move {
        match run_notify(&argv, NOTIFY_TIMEOUT).await {
            Ok(()) => info!(notification = %message, "Notify command ran"),
            Err(e) => warn!(error = %e, notification = %message, "Notify command failed"),
        }
    });
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
            parse_notify_command(text)
                .unwrap()
                .map(|config| config.command),
            Some(vec!["notify-send".into(), "atm".into(), "--".into()])
        );
    }

    #[test]
    fn delay_secs_defaults_to_zero() {
        let delay = |text: &str| parse_notify_command(text).unwrap().map(|c| c.delay_secs);
        assert_eq!(delay("[notify]\ncommand = ['n']\n"), Some(0));
        assert_eq!(
            delay("[notify]\ncommand = ['n']\ndelay_secs = 10\n"),
            Some(10)
        );
        for text in [
            "[notify]\ncommand = ['n']\ndelay_secs = -1\n",
            "[notify]\ncommand = ['n']\ndelay_secs = '10'\n",
            "[notify]\ncommand = ['n']\ndelay = 10\n",
        ] {
            assert!(parse_notify_command(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn no_section_or_an_empty_command_turns_notify_off() {
        for text in [
            "",
            "[harness]\ndefault = 'claude'\n",
            "[notify]\n",
            "[notify]\ncommand = []\n",
        ] {
            assert!(parse_notify_command(text).unwrap().is_none(), "{text:?}");
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

    /// With a delay, a pending notification goes out only if its stretch
    /// of needing input is still open when the delay ends.
    #[test]
    fn a_stretch_stays_open_until_the_session_leaves_needs_input() {
        let mut tracker = AttentionTracker::default();
        let first = tracker
            .observe(&updated("a", AttentionNeeded))
            .expect("entering needs input notifies");
        assert_eq!(first.session_id, SessionId::new("a"));
        assert!(first.message.contains("needs input"), "{}", first.message);
        assert!(tracker.is_open(&first.session_id, first.stretch));

        // Staying there keeps the same stretch.
        assert!(tracker.observe(&updated("a", AttentionNeeded)).is_none());
        assert!(tracker.is_open(&first.session_id, first.stretch));

        // Leaving ends it, and coming back starts a new one.
        tracker.observe(&updated("a", Working));
        assert!(!tracker.is_open(&first.session_id, first.stretch));
        let second = tracker
            .observe(&updated("a", AttentionNeeded))
            .expect("a new dialog notifies");
        assert_ne!(second.stretch, first.stretch);
        assert!(!tracker.is_open(&first.session_id, first.stretch));
        assert!(tracker.is_open(&second.session_id, second.stretch));

        // Removed and Registered end it too.
        tracker.observe(&removed("a"));
        assert!(!tracker.is_open(&second.session_id, second.stretch));
        let third = tracker.observe(&updated("a", AttentionNeeded)).unwrap();
        tracker.observe(&registered("a"));
        assert!(!tracker.is_open(&third.session_id, third.stretch));

        // Another session's stretch is its own.
        let other = tracker.observe(&updated("b", AttentionNeeded)).unwrap();
        assert!(!tracker.is_open(&SessionId::new("a"), other.stretch));
    }

    /// The notifier loop with a 400 ms delay, in real time: three sessions
    /// whose calls a stand-in records, one file per call.
    #[tokio::test]
    async fn the_delay_skips_a_dialog_answered_in_time() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record");
        std::fs::write(
            &record,
            format!(
                "#!/bin/sh\nf=$(mktemp '{}/call.XXXXXX') && printf '%s' \"$1\" > \"$f\"\n",
                dir.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&record, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let calls = |id: &str| {
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|entry| std::fs::read_to_string(entry.unwrap().path()).ok())
                .filter(|text| text.contains(&format!(" {id}: ")))
                .count()
        };
        let delay = Duration::from_millis(400);
        let (events, receiver) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let command = vec![record.display().to_string()];
        let notifier = spawn_notifier(receiver, command, delay, cancel.clone());
        let send = |event| {
            events.send(event).unwrap();
        };
        let pause = |ms| tokio::time::sleep(Duration::from_millis(ms));

        // `answered` is answered at 50 ms; `open` is left open; `again` is
        // answered at 50 ms and reopened at 250 ms, before its first timer
        // ends, so only the second stretch may notify, 400 ms after 250.
        send(updated("answered", AttentionNeeded));
        send(updated("open", AttentionNeeded));
        send(updated("again", AttentionNeeded));
        pause(50).await;
        send(updated("answered", Working));
        send(updated("again", Working));
        pause(200).await;
        send(updated("again", AttentionNeeded));
        let reopened = Instant::now();
        assert_eq!(calls("open"), 0, "notified before the delay");

        let deadline = Instant::now() + Duration::from_secs(3);
        while calls("again") == 0 && Instant::now() < deadline {
            pause(10).await;
        }
        assert_eq!(calls("again"), 1);
        assert!(
            reopened.elapsed() >= delay - Duration::from_millis(20),
            "again notified {:?} after reopening",
            reopened.elapsed()
        );
        pause(600).await;
        assert_eq!(
            (calls("answered"), calls("open"), calls("again")),
            (0, 1, 1)
        );
        cancel.cancel();
        notifier.await.unwrap();
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
        assert!(matches!(err, NotifyError::Io { .. }), "{err}");
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
