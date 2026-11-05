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
//! in time sends nothing.
//!
//! Every row notifies on its own, so what a dialog sends follows from the
//! rows the harness's hooks mark as needing input, recorded in each
//! adapter's crate docs. For Claude Code (atm-claude-adapter): one dialog
//! notifies once, an approved dialog's row needs input until its tool
//! ends, so a tool that outlasts the delay still notifies, and a
//! subagent's dialog that stays open marks its lead's row too, so it
//! notifies twice. Those fixes belong where the hooks are mapped. A
//! row-level mute for the double ping was tried here and dropped: on
//! 2026-11-04 it lost a lead's own dialogs (#129).

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
    /// atm's `config.toml` is not valid, or its `[notify]` is not. Shown
    /// without toml's quote of the line, which can hold a webhook URL.
    #[error("invalid config: {}", .0.message())]
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

/// Remembers which sessions need input, to notify only on the change.
#[derive(Debug, Default)]
pub struct AttentionTracker {
    /// Each session that needs input, and the id of that stretch.
    in_attention: HashMap<SessionId, u64>,
    last_stretch: u64,
}

impl AttentionTracker {
    /// The notification to send when `event` turns a session into needing
    /// input.
    ///
    /// `Registered` carries no status, so it only resets the session to
    /// "not needing input"; the next `Updated` decides. Every row counts on
    /// its own, so a subagent's dialog that stays open notifies again for
    /// its lead, which Claude marks as waiting too.
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
                self.in_attention
                    .insert(session.id.clone(), self.last_stretch);
                Some(Pending {
                    session_id: session.id.clone(),
                    stretch: self.last_stretch,
                    message: notification_message(session),
                })
            }
            SessionEvent::Updated { session } => {
                self.in_attention.remove(&session.id);
                None
            }
        }
    }

    /// Whether `session_id` still needs input in the same `stretch`.
    pub fn is_open(&self, session_id: &SessionId, stretch: u64) -> bool {
        self.in_attention.get(session_id) == Some(&stretch)
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
        if let Some(mut pipe) = pipe {
            // Keep the start and drop the rest: memory stays bounded, and
            // the pipe stays open, so a chatty command isn't killed by SIGPIPE.
            (&mut pipe)
                .take(STDERR_LIMIT as u64)
                .read_to_end(&mut stderr)
                .await?;
            tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await?;
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
                    Err(RecvError::Lagged(skipped)) => {
                        // A dropped "left needs input" would mute that session's
                        // next dialog; forget them all (at worst one extra ping).
                        // Stretch ids keep counting, so no pending timer matches
                        // a stretch that starts after this.
                        warn!(skipped, "Notifier missed events");
                        tracker.in_attention.clear();
                    }
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

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// Each config text, and the command and delay it turns on, `None`
    /// (notify off), or the text its error must contain.
    #[test]
    fn parses_the_notify_config() {
        type Parsed = Result<Option<(Vec<String>, u64)>, &'static str>;
        let on = |command: &[&str], delay: u64| -> Parsed {
            Ok(Some((
                command.iter().map(|a| a.to_string()).collect(),
                delay,
            )))
        };
        let invalid = || -> Parsed { Err("invalid config: ") };
        let cases: [(&str, Parsed); 16] = [
            (
                "[harness]\ndefault = 'pi'\n\n[notify]\ncommand = ['notify-send', 'atm', '--']\n",
                on(&["notify-send", "atm", "--"], 0),
            ),
            ("[notify]\ncommand = ['n']\n", on(&["n"], 0)),
            ("[notify]\ncommand = ['n']\ndelay_secs = 10\n", on(&["n"], 10)),
            // No section or an empty command turns notify off.
            ("", Ok(None)),
            ("[harness]\ndefault = 'claude'\n", Ok(None)),
            ("[notify]\n", Ok(None)),
            ("[notify]\ncommand = []\n", Ok(None)),
            ("[notify]\ncommand = 'notify-send atm'\n", invalid()),
            ("[notify]\ncommand = [1]\n", invalid()),
            ("[notify\n", invalid()),
            ("[notify]\ncommand = ['n']\ndelay_secs = -1\n", invalid()),
            ("[notify]\ncommand = ['n']\ndelay_secs = '10'\n", invalid()),
            // A typo inside [notify] is an error, not notifications off.
            ("[notify]\ncommand = ['n']\ndelay = 10\n", invalid()),
            ("[notify]\ncmd = ['notify-send', 'atm', '--']\n", invalid()),
            // atmd logs a config error, so it must not quote the offending
            // line: that line can hold a webhook URL or an ntfy topic.
            (
                "[notify]\ncmd = ['curl', 'https://hooks.example.invalid/services/T0/B0/SECRET']\n",
                Err("unknown field `cmd`"),
            ),
            (
                "[notify]\ncommand = ['curl', 'https://hooks.example.invalid/services/T0/B0/SECRET'\n",
                invalid(),
            ),
        ];
        for (text, want) in cases {
            let got = parse_notify_command(text)
                .map(|config| config.map(|c| (c.command, c.delay_secs)))
                .map_err(|e| e.to_string());
            match (got, want) {
                (Err(shown), Err(says)) => assert!(
                    shown.starts_with("invalid config: ")
                        && shown.contains(says)
                        && !shown.contains("SECRET"),
                    "{text:?}: {shown}"
                ),
                (got, want) => assert_eq!(got, want.map_err(String::from), "{text:?}"),
            }
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

    fn update(session: SessionView) -> SessionEvent {
        SessionEvent::Updated {
            session: Box::new(session),
        }
    }

    fn updated(id: &str, status: SessionStatus) -> SessionEvent {
        update(view(id, status))
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

    fn needs(id: &str) -> SessionEvent {
        updated(id, AttentionNeeded)
    }

    fn working(id: &str) -> SessionEvent {
        updated(id, Working)
    }

    /// Event sequences, and whether the tracker fires on each event.
    #[test]
    fn fires_when_a_session_starts_needing_input() {
        // The last case, seen with a real Claude Code 2.1.317 TUI on
        // 2026-11-04 (#150's overlap run): a background subagent asks (its
        // row needs input), then the lead asks for itself 4 s later (its row
        // needs input too). Two dialogs, two notifications: no row mutes
        // another, because a mute can't tell the lead's own dialog from
        // Claude marking the lead for its subagent's dialog, and it lost the
        // lead's ping here.
        let mut lead = view("lead", Working);
        lead.child_session_ids = vec![SessionId::new("sub")];
        let mut sub = view("sub", AttentionNeeded);
        sub.parent_session_id = Some(SessionId::new("lead"));
        let lead_now = |status| {
            update(SessionView {
                status,
                ..lead.clone()
            })
        };
        let idle = |id| updated(id, Idle);
        let cases: [(Vec<SessionEvent>, &[bool]); 8] = [
            (
                vec![registered("a"), idle("a"), needs("a")],
                &[false, false, true],
            ),
            (vec![working("a"), needs("a")], &[false, true]),
            // Quiet while it keeps needing input: Claude sends
            // `PermissionRequest` and then `Notification` (`permission_prompt`)
            // for one dialog, and both map to needs input.
            (
                vec![working("a"), needs("a"), needs("a")],
                &[false, true, false],
            ),
            // Again for the next dialog.
            (
                vec![needs("a"), working("a"), needs("a")],
                &[true, false, true],
            ),
            // `Registered` carries no status, so it only resets the
            // baseline. Discovery's rows start idle, so startup sends nothing.
            (
                vec![registered("a"), idle("a"), registered("b")],
                &[false, false, false],
            ),
            // A session that left and came back starts fresh.
            (
                vec![needs("a"), removed("a"), registered("a"), needs("a")],
                &[true, false, false, true],
            ),
            // Sessions are tracked apart.
            (
                vec![needs("a"), needs("b"), needs("a")],
                &[true, true, false],
            ),
            (
                vec![lead_now(Working), update(sub), lead_now(AttentionNeeded)],
                &[false, true, true],
            ),
        ];
        for (events, want) in cases {
            assert_eq!(fired(&events), want, "{events:?}");
        }
    }

    /// With a delay, a pending notification goes out only if its stretch
    /// of needing input is still open when the delay ends.
    #[test]
    fn a_stretch_stays_open_until_the_session_leaves_needs_input() {
        let mut tracker = AttentionTracker::default();
        let first = tracker
            .observe(&needs("a"))
            .expect("entering needs input notifies");
        assert_eq!(first.session_id, SessionId::new("a"));
        assert!(first.message.contains("needs input"), "{}", first.message);
        assert!(tracker.is_open(&first.session_id, first.stretch));

        // Staying there keeps the same stretch.
        assert!(tracker.observe(&needs("a")).is_none());
        assert!(tracker.is_open(&first.session_id, first.stretch));

        // Leaving ends it, and coming back starts a new one.
        tracker.observe(&working("a"));
        assert!(!tracker.is_open(&first.session_id, first.stretch));
        let second = tracker.observe(&needs("a")).expect("a new dialog notifies");
        assert_ne!(second.stretch, first.stretch);
        assert!(!tracker.is_open(&first.session_id, first.stretch));
        assert!(tracker.is_open(&second.session_id, second.stretch));

        // Removed and Registered end it too.
        tracker.observe(&removed("a"));
        assert!(!tracker.is_open(&second.session_id, second.stretch));
        let third = tracker.observe(&needs("a")).unwrap();
        tracker.observe(&registered("a"));
        assert!(!tracker.is_open(&third.session_id, third.stretch));

        // Another session's stretch is its own.
        let other = tracker.observe(&needs("b")).unwrap();
        assert!(!tracker.is_open(&SessionId::new("a"), other.stretch));
    }

    /// The notifier loop in real time. Its stand-in command, `sh -c`, so
    /// no file is executed, writes each call's message to a file of its own.
    struct Notifier {
        calls: tempfile::TempDir,
        events: broadcast::Sender<SessionEvent>,
        cancel: CancellationToken,
        task: JoinHandle<()>,
    }

    impl Notifier {
        /// A notifier with `delay`, fed by a channel of `capacity` events.
        fn start(delay: Duration, capacity: usize) -> Self {
            let calls = tempfile::tempdir().unwrap();
            let script = format!(
                "f=$(mktemp '{}/call.XXXXXX') && printf '%s' \"$1\" > \"$f\"",
                calls.path().display()
            );
            let command = vec!["sh".into(), "-c".into(), script, "sh".into()];
            let (events, receiver) = broadcast::channel(capacity);
            let cancel = CancellationToken::new();
            let task = spawn_notifier(receiver, command, delay, cancel.clone());
            Self {
                calls,
                events,
                cancel,
                task,
            }
        }

        fn send(&self, event: SessionEvent) {
            self.events.send(event).unwrap();
        }

        /// The message of each call so far.
        fn messages(&self) -> Vec<String> {
            std::fs::read_dir(self.calls.path())
                .unwrap()
                .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
                .collect()
        }

        /// How many calls so far named session `id`.
        fn calls(&self, id: &str) -> usize {
            let named = format!(" {id}: ");
            self.messages()
                .iter()
                .filter(|m| m.contains(&named))
                .count()
        }

        /// Waits up to 10 s for the `n`th call naming `id`.
        async fn wait_for(&self, id: &str, n: usize) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.calls(id) < n && Instant::now() < deadline {
                tokio::time::sleep(ms(10)).await;
            }
            assert_eq!(self.calls(id), n, "calls naming {id}");
        }

        /// Answers `id`'s dialog in a burst that overflows a small channel,
        /// so the notifier lags and never sees the answer.
        async fn lose_answer_in_a_lag(&self, id: &str) {
            self.send(working(id));
            for i in 0..8 {
                self.send(working(&format!("b{i}")));
            }
            tokio::time::sleep(ms(100)).await;
        }

        /// Waits `quiet` for a call that shouldn't come, then stops the
        /// loop and returns how many calls named each of `ids`, which must
        /// be all the calls.
        async fn finish(self, quiet: Duration, ids: &[&str]) -> Vec<usize> {
            tokio::time::sleep(quiet).await;
            let counts: Vec<usize> = ids.iter().map(|id| self.calls(id)).collect();
            let all = self.messages();
            assert_eq!(counts.iter().sum::<usize>(), all.len(), "{all:?}");
            self.cancel.cancel();
            self.task.await.unwrap();
            counts
        }
    }

    /// Checks that a call seen just now came at least `delay` after `since`.
    fn no_sooner_than(since: Instant, delay: Duration) {
        let elapsed = since.elapsed();
        assert!(elapsed >= delay - ms(20), "notified {elapsed:?} after");
    }

    /// With a 400 ms delay: `answered` is answered at 50 ms; `open` is left
    /// open; `again` is answered at 50 ms and reopened at 250 ms, before
    /// its first timer ends, so only the second stretch may notify, 400 ms
    /// after 250.
    #[tokio::test]
    async fn the_delay_skips_a_dialog_answered_in_time() {
        let delay = ms(400);
        let notifier = Notifier::start(delay, 16);
        for id in ["answered", "open", "again"] {
            notifier.send(needs(id));
        }
        tokio::time::sleep(ms(50)).await;
        notifier.send(working("answered"));
        notifier.send(working("again"));
        tokio::time::sleep(ms(200)).await;
        notifier.send(needs("again"));
        let reopened = Instant::now();
        assert_eq!(notifier.calls("open"), 0, "notified before the delay");

        notifier.wait_for("again", 1).await;
        no_sooner_than(reopened, delay);
        let ids = ["answered", "open", "again"];
        assert_eq!(notifier.finish(ms(600), &ids).await, [0, 1, 1]);
    }

    /// A lagging broadcast can drop a session's "left needs input" event.
    /// The notifier then forgets every session, so that session's next
    /// dialog still notifies.
    #[tokio::test]
    async fn a_lagged_notifier_still_notifies_the_next_dialog() {
        let notifier = Notifier::start(Duration::ZERO, 4);
        notifier.send(needs("a"));
        notifier.wait_for("a", 1).await;
        notifier.lose_answer_in_a_lag("a").await;
        notifier.send(needs("a"));
        notifier.wait_for("a", 2).await;
        assert_eq!(notifier.finish(ms(300), &["a"]).await, [2]);
    }

    /// After a lag, a delay timer from before it must not count for a
    /// stretch that starts after it: one notification, a full delay after
    /// the session's new dialog.
    #[tokio::test]
    async fn a_timer_from_before_a_lag_does_not_fire_for_a_new_stretch() {
        let delay = ms(400);
        let notifier = Notifier::start(delay, 4);
        notifier.send(needs("a"));
        tokio::time::sleep(ms(50)).await;
        notifier.lose_answer_in_a_lag("a").await;
        notifier.send(needs("a"));
        let reopened = Instant::now();

        notifier.wait_for("a", 1).await;
        no_sooner_than(reopened, delay);
        assert_eq!(notifier.finish(ms(800), &["a"]).await, [1]);
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

    /// Runs `script` with `sh -c` through `run_notify`.
    async fn run_sh(script: &str, timeout: Duration) -> Result<(), NotifyError> {
        run_notify(&["sh", "-c", script].map(String::from), timeout).await
    }

    #[tokio::test]
    async fn run_notify_reports_a_command_that_fails() {
        let err = run_sh("echo 'webhook said no' >&2; exit 3", Duration::from_secs(5))
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
        let script = "head -c 1000000 /dev/zero | tr '\\0' x >&2; exit 1";
        let err = run_sh(script, Duration::from_secs(10))
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

    /// A command that keeps writing to stderr past the limit still runs to
    /// the end: atmd drops the rest instead of closing the pipe, which
    /// killed such a command with SIGPIPE before it could send anything.
    #[tokio::test]
    async fn run_notify_lets_a_chatty_command_finish() {
        let script = "printf '%5000s' x >&2; sleep 0.3; echo more >&2; exit 0";
        run_sh(script, Duration::from_secs(10))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
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
        run_sh("exit 0", Duration::from_secs(5)).await.unwrap();
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
        let started = Instant::now();
        let err = run_sh(&format!("exec sleep {marker}"), ms(200))
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
