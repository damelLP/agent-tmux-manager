//! Pure decisions of the named-agent lifecycle flows.
//!
//! Nothing here does I/O, awaits or reads a clock: each function maps what one
//! capture showed, plus the little a flow remembers, to the next step. The
//! flows own every effect and re-check each key on the control actor's own
//! capture. No harness is named: all vendor knowledge arrives as the adapter's
//! [`ControlPolicy`] and [`ControlScreen`].

use super::agent_screen::{AgentScreen, Foreground, PaneBaseline};
use crate::control::reservation::{CaptureFormat, PaneCapture, PaneKey};
use crate::registry::PaneSession;
use atm_core::harness_control::{
    ClearProof, ControlDialog, ControlKey, ControlPolicy, ControlScreen, SessionArrival,
};
use atm_core::styled_screen::StyledScreen;
use atm_core::{DialogState, FreshEditorEvidence, InputState, SessionId, TurnState};
use std::path::PathBuf;

/// What a flow does after one capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Step {
    /// Send nothing and look again. The text says what is being waited for; at
    /// the flow's deadline it becomes the reason the agent is stuck.
    Wait(&'static str),
    /// Send `key`, guarded: the control actor's own capture must show exactly
    /// the dialog `expect` (same folder, same selection, same keys).
    Key {
        key: PaneKey,
        expect: ControlDialog,
        action: KeyAction,
    },
    Ready(Ready),
    /// Stop: send nothing more and leave the pane for its owner.
    Stuck(String),
}

/// Which key of a start a [`Step::Key`] is; reported back with
/// [`StartProgress::sent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyAction {
    /// Move a trust dialog's selection to its accepting option.
    TrustSelect,
    /// Accept a trust dialog whose accepting option is selected.
    TrustAccept,
    /// Dismiss a prompt the launch may skip (an update offer).
    SkipLaunchPrompt,
    /// Submit the command staged in the editor.
    Submit,
    /// Keep the current worktree when a clear asks which one to use.
    KeepWorktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ready {
    /// The harness announced this session and shows a safe editor.
    Session(SessionId),
    /// A fresh, safe editor; the session arrives with the first prompt.
    Editor,
}

/// The keys one start has sent. A key whose outcome is unknown counts as sent.
#[derive(Debug, Clone, Default)]
pub(super) struct StartProgress {
    last_trust: Option<KeyAction>,
    accepts: u8,
    folder: Option<String>,
    launch_skipped: bool,
    trust_reported: bool,
}

impl StartProgress {
    /// Record that `action` was sent on the dialog `expect`.
    pub(super) fn sent(&mut self, action: KeyAction, expect: &ControlDialog) {
        match action {
            KeyAction::SkipLaunchPrompt => self.launch_skipped = true,
            // Keys of a clear; a start sends neither.
            KeyAction::Submit | KeyAction::KeepWorktree => {}
            KeyAction::TrustSelect | KeyAction::TrustAccept => {
                if let ControlDialog::FolderTrust { folder, .. } = expect {
                    self.folder.get_or_insert_with(|| folder.clone());
                }
                self.last_trust = Some(action);
                if action == KeyAction::TrustAccept {
                    self.accepts = self.accepts.saturating_add(1);
                    self.trust_reported = false;
                }
            }
        }
    }

    /// The folder whose trust dialog this start accepted, once: `Some` on the
    /// first recognised screen after an accept that is not that dialog. The
    /// caller audits the acceptance then.
    pub(super) fn trust_accepted(&mut self, control: &ControlScreen) -> Option<String> {
        if self.last_trust != Some(KeyAction::TrustAccept)
            || self.trust_reported
            || matches!(
                control.dialog,
                ControlDialog::Unknown | ControlDialog::FolderTrust { .. }
            )
        {
            return None;
        }
        self.trust_reported = true;
        self.folder.clone()
    }
}

/// The only keys the control actor sends; a literal character has no route yet.
#[must_use]
pub(super) fn pane_key(key: ControlKey) -> Option<PaneKey> {
    match key {
        ControlKey::Enter => Some(PaneKey::Enter),
        ControlKey::Escape => Some(PaneKey::Escape),
        ControlKey::Up => Some(PaneKey::Up),
        ControlKey::Down => Some(PaneKey::Down),
        _ => None,
    }
}

/// What one capture shows of a start, between the launch and readiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StartView {
    /// Nothing to act on; the text is the reason, as in [`Step::Wait`].
    Wait(&'static str),
    /// A command other than the shell shows its first recognised screen: the
    /// harness's name is pinned from here on. Decide on `control` next.
    Pin {
        baseline: PaneBaseline,
        control: ControlScreen,
    },
    /// The pinned harness is in the foreground.
    Harness(ControlScreen),
    Stuck(String),
}

/// Read a start-phase capture against the pane's baseline.
///
/// The shell in front before a harness is pinned is the moment right after
/// the launch Enter, so it waits; once pinned it means the harness exited.
/// A command that is neither waits too: a launcher that has not become the
/// harness yet (an npm wrapper shows `node` first) draws no harness screen,
/// so it pins nothing.
#[must_use]
pub(super) fn start_view(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
) -> StartView {
    let pinned = baseline.harness_command().is_some();
    match screen.foreground(capture, baseline) {
        Some(Foreground::Harness) => StartView::Harness(screen.control(capture, baseline)),
        Some(Foreground::Shell) if pinned => {
            StartView::Stuck("the harness exited to its shell during the start".into())
        }
        Some(Foreground::Shell) => StartView::Wait("the launch has started no harness"),
        None => match screen.start_candidate(capture, baseline) {
            Some((command, control)) if control.dialog != ControlDialog::Unknown => {
                match baseline.clone().with_harness(command) {
                    Some(baseline) => StartView::Pin { baseline, control },
                    None => StartView::Wait("the harness could not be told from its shell"),
                }
            }
            Some(_) => StartView::Wait("no harness screen was recognised"),
            None if pinned => {
                StartView::Wait("the pane's foreground command changed after the harness started")
            }
            None => StartView::Wait("the pane's processes could not be read"),
        },
    }
}

/// Decide a start's next step from the harness's control screen.
///
/// `trust_dirs` are the role's canonical trust directories; a trust dialog is
/// answered only for a folder that equals one of them as a string. `session` is
/// the new session the harness announced on this pane, if it has.
#[must_use]
pub(super) fn decide_start(
    policy: &ControlPolicy,
    control: &ControlScreen,
    progress: &StartProgress,
    trust_dirs: &[PathBuf],
    session: Option<&SessionId>,
) -> Step {
    match &control.dialog {
        ControlDialog::Unknown => Step::Wait("the screen was not recognised"),
        ControlDialog::FolderTrust {
            folder,
            selected_accept,
            select_accept,
            accept,
        } => trust(
            policy,
            control,
            progress,
            trust_dirs,
            (folder, *selected_accept, *select_accept, *accept),
        ),
        _ if progress.last_trust == Some(KeyAction::TrustSelect) => {
            Step::Stuck("the trust dialog went away without being accepted by atm".into())
        }
        ControlDialog::LaunchSkip { .. } if progress.launch_skipped => {
            Step::Wait("the update prompt is still shown after it was skipped")
        }
        ControlDialog::LaunchSkip { key } => match pane_key(*key) {
            Some(key) => Step::Key {
                key,
                expect: control.dialog.clone(),
                action: KeyAction::SkipLaunchPrompt,
            },
            None => Step::Stuck("the update prompt needs a key atm cannot send".into()),
        },
        ControlDialog::None => ready(policy, control, session),
        ControlDialog::HookReview { .. } => Step::Stuck(
            "the harness asks to review its hooks; review them in the pane (atm cannot trust them yet)"
                .into(),
        ),
        ControlDialog::Blocked => Step::Stuck("a dialog atm must not answer is open".into()),
        _ => Step::Stuck("an unexpected dialog is open during the start".into()),
    }
}

/// The trust dialog's table: at most one select and one accept per round, and
/// one round more than the policy's retries. Accept is only ever sent on a
/// capture that shows the accepting option selected.
fn trust(
    policy: &ControlPolicy,
    control: &ControlScreen,
    progress: &StartProgress,
    trust_dirs: &[PathBuf],
    (folder, selected, select, accept): (&String, bool, Option<ControlKey>, ControlKey),
) -> Step {
    if !trust_dirs
        .iter()
        .any(|dir| dir.as_os_str() == folder.as_str())
    {
        return Step::Stuck(format!(
            "the harness asks to trust {folder}, which is not one of the role's trust directories"
        ));
    }
    if progress
        .folder
        .as_ref()
        .is_some_and(|first| first != folder)
    {
        return Step::Stuck(format!(
            "the trust dialog changed its folder to {folder} during the start"
        ));
    }
    let rounds_left = progress.accepts <= policy.trust_selection_retries;
    let key = |key: ControlKey, action| match pane_key(key) {
        Some(key) => Step::Key {
            key,
            expect: control.dialog.clone(),
            action,
        },
        None => Step::Stuck("the trust dialog needs a key atm cannot send".into()),
    };
    match (selected, progress.last_trust) {
        (false, Some(KeyAction::TrustSelect)) => {
            Step::Wait("the trust dialog does not show the accepting option selected yet")
        }
        (true, Some(KeyAction::TrustAccept)) => {
            Step::Wait("the trust dialog is still shown after it was accepted")
        }
        (false, Some(KeyAction::TrustAccept)) if !rounds_left => {
            Step::Stuck("the trust dialog reset again after it was accepted".into())
        }
        (false, _) => match select {
            Some(select) => key(select, KeyAction::TrustSelect),
            None => Step::Stuck(
                "the trust dialog does not have its accepting option selected and offers no key to select it"
                    .into(),
            ),
        },
        (true, _) if rounds_left => key(accept, KeyAction::TrustAccept),
        (true, _) => Step::Stuck("the trust dialog was already accepted as often as allowed".into()),
    }
}

fn ready(policy: &ControlPolicy, control: &ControlScreen, session: Option<&SessionId>) -> Step {
    let facts = &control.facts;
    if facts.input != InputState::Empty
        || facts.turn != TurnState::NotRunning
        || facts.dialog != DialogState::None
    {
        return Step::Wait("the editor is not empty and idle");
    }
    match policy.session_arrival {
        SessionArrival::AtStart => match session {
            Some(session) => Step::Ready(Ready::Session(session.clone())),
            None => Step::Wait("the harness has not announced its session"),
        },
        SessionArrival::AtFirstPrompt if control.conversation => {
            Step::Stuck("a conversation is on screen in a harness that was just started".into())
        }
        SessionArrival::AtFirstPrompt => Step::Ready(Ready::Editor),
        _ => Step::Stuck("the harness announces its session in a way atm does not know".into()),
    }
}

/// Whether a launch line may be pasted into a shell at all: printable ASCII
/// only, so every character is one terminal cell and the staged line can be
/// compared exactly. Anything else refuses the launch before any effect.
#[must_use]
pub(super) fn launch_text_is_safe(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| matches!(c, ' '..='~'))
}

/// The terminal cells one character takes, when that is certain: one or two.
/// `None` for a control character and for a character without a cell of its
/// own (a combining mark, a joiner, a variation selector): terminals lay
/// those out differently, and nothing here guesses.
fn cells(c: char) -> Option<usize> {
    unicode_width::UnicodeWidthChar::width(c).filter(|cells| (1..=2).contains(cells))
}

/// The cells before the cursor on the cursor's logical line: the cursor's row
/// up to the cursor, joined upward with every row that fills the pane's width
/// (a wrapped line, or a row its shell filled: the join is not a proof of a
/// wrap, only the same reading before and after a paste). Cells at or after
/// the cursor are ignored: a shell leaves stale cells, suggestions and a
/// right-hand prompt there.
///
/// A capture holds one character per glyph, so the cut is made by cell widths
/// and must agree with the cursor column tmux reports. `None` when the line
/// can't be read exactly: an undecodable capture, a cursor outside it, a
/// character before the cursor without a certain width, a cursor column that
/// falls inside a character, or a row with more cells than the pane is wide.
#[must_use]
pub(super) fn launch_line(capture: &PaneCapture) -> Option<String> {
    let decoded;
    let text = match capture.format {
        CaptureFormat::Plain => capture.text.as_str(),
        CaptureFormat::Ansi => {
            decoded = StyledScreen::parse(&capture.text).ok()?;
            decoded.text()
        }
    };
    let rows: Vec<&str> = text.lines().collect();
    let width = usize::from(capture.inspection.pane.width);
    let column = usize::try_from(capture.inspection.cursor_column).ok()?;
    let row = usize::try_from(capture.inspection.cursor_row).ok()?;
    if width == 0 || column > width {
        return None;
    }
    // A cursor column equal to the width is the end of that row (dash leaves
    // it there after an exact fit; bash and zsh move to the next row).
    let cursor_row = rows.get(row)?;
    let mut line = String::new();
    let mut before = 0;
    for c in cursor_row.chars() {
        if before >= column {
            break;
        }
        before += cells(c)?;
        line.push(c);
    }
    let all: Option<usize> = cursor_row.chars().map(cells).sum();
    if before > column || all.is_some_and(|all| all > width) {
        return None;
    }
    // Cells never written before the cursor are blank.
    line.extend(std::iter::repeat_n(' ', column - before));
    for above in rows.get(..row)?.iter().rev() {
        // The row's cells, at least and at most: a character without a
        // certain width may take none or two.
        let (mut least, mut most) = (0, 0);
        for c in above.chars() {
            least += cells(c).unwrap_or(0);
            most += cells(c).unwrap_or(2);
        }
        if least == width && most == width {
            line.insert_str(0, above);
        } else if most < width {
            break;
        } else {
            return None;
        }
    }
    Some(line)
}

/// The Enter guard of a launch: the line reads exactly what the shell showed
/// before the paste (`before`) followed by the launch text. Equality, never
/// "ends with": text typed ahead of the paste would otherwise run with it.
#[must_use]
pub(super) fn launch_line_is_staged(capture: &PaneCapture, before: &str, text: &str) -> bool {
    launch_line(capture).is_some_and(|line| {
        line.len() == before.len() + text.len() && line.starts_with(before) && line.ends_with(text)
    })
}

/// Why a capture does not allow typing a launch into the pane's shell; `None`
/// allows. Both guards of a launch require this of the control actor's own
/// capture: the agent's tag, a live pane out of copy mode, the recorded shell
/// in the foreground of the recorded root process, no client focused on the
/// pane (a person may be typing there), and no harness screen on it.
#[must_use]
pub(super) fn shell_refusal(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
    tag: &str,
) -> Option<&'static str> {
    let pane = &capture.inspection;
    if pane.local_agent_tag.as_deref() != Some(tag) {
        Some("the pane no longer carries the agent's tag")
    } else if pane.dead {
        Some("the shell in the pane has exited")
    } else if pane.in_mode {
        Some("the pane is in copy mode")
    } else if screen.foreground(capture, baseline) != Some(Foreground::Shell) {
        Some("the pane's shell is not known to be its foreground program")
    } else if screen.recognises(capture) {
        Some("the pane shows a harness's screen before anything was launched")
    } else {
        match capture.focused {
            Some(false) => None,
            Some(true) => Some("the pane is focused"),
            None => Some("whether the pane is focused could not be read"),
        }
    }
}

/// The paste guard of a launch: the shell's line still reads exactly `before`.
#[must_use]
pub(super) fn launch_paste_allowed(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
    tag: &str,
    before: &str,
) -> bool {
    shell_refusal(screen, capture, baseline, tag).is_none()
        && launch_line(capture).as_deref() == Some(before)
}

/// The Enter guard of a launch, on the control actor's own capture.
#[must_use]
pub(super) fn launch_enter_allowed(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
    tag: &str,
    (before, text): (&str, &str),
) -> bool {
    shell_refusal(screen, capture, baseline, tag).is_none()
        && launch_line_is_staged(capture, before, text)
}

/// Where a role's `pane` hint puts the agent's new pane: `<side>:<agent>`,
/// the side of that agent's pane to split off (`below:lead`).
#[must_use]
pub(super) fn placement(hint: &str) -> Option<(atm_tmux::PaneDirection, &str)> {
    use atm_tmux::PaneDirection::{Above, Below, Left, Right};
    let (side, agent) = hint.split_once(':')?;
    let direction = match side {
        "left" => Left,
        "right" => Right,
        "above" => Above,
        "below" => Below,
        _ => return None,
    };
    (!agent.is_empty()).then_some((direction, agent))
}

/// The session a start produced, among the rows on the agent's new pane: its
/// one top-level session of the agent's harness, once the registry has
/// accepted its start. `Ok(None)` waits: no such row yet, or a row something
/// other than a start created. More than one is an error; nothing says which
/// is the agent's.
pub(super) fn started_session(
    harness: &str,
    rows: &[PaneSession],
) -> Result<Option<SessionId>, String> {
    let mut own = rows.iter().filter(|row| {
        row.view.parent_session_id.is_none()
            && !row.view.id.is_pending()
            && row.view.harness == harness
    });
    let Some(row) = own.next() else {
        return Ok(None);
    };
    if own.next().is_some() {
        return Err("more than one session appeared on the new pane".into());
    }
    Ok(row.start.as_ref().map(|_| row.view.id.clone()))
}

/// What procfs says about a pane's root process. A flow reads it once, when it
/// records a baseline or checks for a return to the shell; never a guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProcFacts {
    /// The root's process group.
    pub process_group: i32,
    /// The foreground process group of the root's terminal.
    pub foreground_group: i32,
    /// The root's executable is recognized as a shell for pane detection.
    pub system_shell: bool,
    /// How many child processes the root has.
    pub children: usize,
    /// The root's name as tmux shows it: the basename of its `argv[0]`
    /// without a login shell's leading `-`.
    pub name: String,
}

impl ProcFacts {
    /// Build the facts from what was read: the content of `/proc/<pid>/stat`,
    /// of `/proc/<pid>/cmdline` and of `/proc/<pid>/task/<pid>/children`, the
    /// canonical path of `/proc/<pid>/exe`, and the resolved shell paths.
    /// `None` when procfs data does not parse. The reader handles an
    /// unavailable shell list separately.
    #[must_use]
    pub(super) fn parse(
        stat: &str,
        cmdline: &[u8],
        children: &str,
        exe: &std::path::Path,
        shells: &[PathBuf],
    ) -> Option<Self> {
        // The command name in `stat` may hold spaces and parentheses; the
        // numeric fields follow its last `)`: state, ppid, pgrp, session,
        // tty_nr, tpgid.
        let mut fields = stat.rsplit_once(')')?.1.split_ascii_whitespace().skip(2);
        let process_group: i32 = fields.next()?.parse().ok()?;
        let foreground_group: i32 = fields.nth(2)?.parse().ok()?;
        let argv0 = std::str::from_utf8(cmdline.split(|byte| *byte == 0).next()?).ok()?;
        let name = argv0.rsplit('/').next()?;
        let name = name.strip_prefix('-').unwrap_or(name);
        let mut count = 0;
        for child in children.split_ascii_whitespace() {
            child.parse::<u32>().ok()?;
            count += 1;
        }
        (process_group > 0 && !name.is_empty()).then(|| Self {
            process_group,
            foreground_group,
            system_shell: shells.iter().any(|shell| shell == exe),
            children: count,
            name: name.to_owned(),
        })
    }
}

/// Which of four shapes a pane's processes are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneShape {
    /// A job other than the root holds the terminal (a shell with job control
    /// running a program).
    JobInFront,
    /// The root holds the terminal and is not a shell: the pane's command is
    /// the program itself.
    RootProgram,
    /// A shell holds the terminal and has children: a program run without job
    /// control. Which of the two is in front can't be told.
    ShellHoldingChild,
    /// A shell holds the terminal and has no children: it is at its prompt.
    IdleShell,
}

/// Classify a pane. `None` when the terminal has no foreground group.
#[must_use]
pub(super) fn classify_pane(facts: &ProcFacts) -> Option<PaneShape> {
    if facts.foreground_group <= 0 {
        None
    } else if facts.foreground_group != facts.process_group {
        Some(PaneShape::JobInFront)
    } else if !facts.system_shell {
        Some(PaneShape::RootProgram)
    } else if facts.children > 0 {
        Some(PaneShape::ShellHoldingChild)
    } else {
        Some(PaneShape::IdleShell)
    }
}

/// The baseline of a pane a launch is about to type into: its root is an idle
/// shell, and tmux names the foreground program as that shell.
#[must_use]
pub(super) fn shell_baseline(
    facts: &ProcFacts,
    root_pid: u32,
    command: &str,
) -> Option<PaneBaseline> {
    (classify_pane(facts) == Some(PaneShape::IdleShell) && command == facts.name)
        .then(|| PaneBaseline::shell(root_pid, command.to_owned()))
}

/// The baseline of a pane that already runs something (an adopt, or the pass
/// after a daemon restart), before any harness name is pinned: a foreground
/// job whose name is not its shell's, or a program that is the pane's root.
/// A shell holding a child without job control, and an idle shell, get none.
#[must_use]
pub(super) fn adopt_baseline(
    facts: &ProcFacts,
    root_pid: u32,
    command: &str,
) -> Option<PaneBaseline> {
    match classify_pane(facts)? {
        PaneShape::JobInFront if command != facts.name => Some(PaneBaseline::adopted(root_pid)),
        PaneShape::RootProgram => Some(PaneBaseline::adopted(root_pid)),
        _ => None,
    }
}

/// A no-effect input refusal. Only a known transient obstruction is retried
/// during a handover's preflight; loss of identity or unknown focus is not.
#[derive(Clone, Copy)]
pub(super) enum InputBlocker {
    Temporary(&'static str),
    Terminal(&'static str),
}

impl InputBlocker {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::Temporary(reason) | Self::Terminal(reason) => reason,
        }
    }
}

/// Why a capture does not allow input to the agent's harness, whatever its
/// screen shows; `None` allows. Every paste and key of a flow requires this of
/// the control actor's own capture: the agent's tag, a live pane out of copy
/// mode, the pinned harness in the foreground, and no client focused on the
/// pane (a person may be typing there). Unknown focus is not permission.
#[must_use]
pub(super) fn input_refusal(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
    tag: &str,
) -> Option<&'static str> {
    input_blocker(screen, capture, baseline, tag).map(InputBlocker::reason)
}

pub(super) fn input_blocker(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
    tag: &str,
) -> Option<InputBlocker> {
    use InputBlocker::{Temporary, Terminal};
    let pane = &capture.inspection;
    if pane.local_agent_tag.as_deref() != Some(tag) {
        Some(Terminal("the pane no longer carries the agent's tag"))
    } else if pane.dead {
        Some(Terminal("the program in the pane has exited"))
    } else if pane.in_mode {
        Some(Temporary("the pane is in copy mode"))
    } else if screen.foreground(capture, baseline) != Some(Foreground::Harness) {
        Some(Terminal(
            "the harness is not known to be the pane's foreground program",
        ))
    } else {
        match capture.focused {
            Some(false) => None,
            Some(true) => Some(Temporary("the pane is focused")),
            None => Some(Terminal("whether the pane is focused could not be read")),
        }
    }
}

/// An editor a command may be typed into: no dialog, empty, no turn running.
#[must_use]
pub(super) fn safe_editor(control: &ControlScreen) -> bool {
    control.dialog == ControlDialog::None
        && control.facts.input == InputState::Empty
        && control.facts.turn == TurnState::NotRunning
        && control.facts.dialog == DialogState::None
}

/// Why a screen can't be cleared; `None` allows. A harness whose clear is
/// proven by its screen alone must show a conversation first: without one
/// there is no change a later fresh editor could prove.
#[must_use]
pub(super) fn clear_refusal(
    policy: &ControlPolicy,
    control: &ControlScreen,
) -> Option<&'static str> {
    if !safe_editor(control) {
        Some("the editor is not empty and idle")
    } else if matches!(policy.clear_proof, ClearProof::ConversationToFresh { .. })
        && !control.conversation
    {
        Some("no conversation is on screen, so a clear could not be confirmed")
    } else {
        None
    }
}

/// A clear with nothing to do: the harness says a ready agent without a
/// session needs none, the agent has no session, no session on its pane could
/// be its own, and the screen is a fresh, safe editor. All of it, or the
/// command is sent.
#[must_use]
pub(super) fn clear_noop(
    policy: &ControlPolicy,
    bound: bool,
    eligible_rows: usize,
    control: &ControlScreen,
) -> bool {
    matches!(
        policy.clear_proof,
        ClearProof::ConversationToFresh {
            ready_unbound_noop: true
        }
    ) && !bound
        && eligible_rows == 0
        && safe_editor(control)
        && control.facts.fresh_editor == FreshEditorEvidence::Present
        && !control.conversation
}

/// The paste guard of a clear, on the control actor's own capture.
#[must_use]
pub(super) fn safe_to_paste(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
    tag: &str,
) -> bool {
    input_refusal(screen, capture, baseline, tag).is_none()
        && clear_refusal(screen.policy(), &screen.control(capture, baseline)).is_none()
}

/// Rule for the Enter that submits a pasted command, over what one capture
/// read: why the pane allows no input (if anything), whether the editor holds
/// exactly the command, and the dialog. Enter needs all three in its favour.
/// A completion popup counts as no dialog only where the adapter recognises
/// its exact layout; an unrecognised one is Unknown and is looked at again. A
/// paste is never repeated and nothing is cleaned up, so a draft that is not
/// the command waits for its owner.
#[must_use]
pub(super) fn decide_submit(
    refusal: Option<&'static str>,
    staged: Option<bool>,
    dialog: &ControlDialog,
) -> Step {
    if let Some(reason) = refusal {
        return Step::Wait(reason);
    }
    match (dialog, staged) {
        (ControlDialog::None, Some(true)) => Step::Key {
            key: PaneKey::Enter,
            expect: ControlDialog::None,
            action: KeyAction::Submit,
        },
        (ControlDialog::None, _) => Step::Wait("the command is not staged in the editor"),
        (ControlDialog::Unknown, _) => Step::Wait("the screen was not recognised"),
        _ => Step::Stuck("a dialog opened before the command was submitted".into()),
    }
}

/// [`decide_submit`] on one capture: `expected` must be staged exactly,
/// leading whitespace included.
#[must_use]
pub(super) fn submit_step(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
    tag: &str,
    expected: &str,
) -> Step {
    decide_submit(
        input_refusal(screen, capture, baseline, tag),
        screen.staged(capture, baseline, expected),
        &screen.control(capture, baseline).dialog,
    )
}

/// The guard of the submitting Enter, on the control actor's own capture.
#[must_use]
pub(super) fn submit_allowed(
    screen: &AgentScreen,
    capture: &PaneCapture,
    baseline: &PaneBaseline,
    tag: &str,
    expected: &str,
) -> bool {
    matches!(
        submit_step(screen, capture, baseline, tag, expected),
        Step::Key { .. }
    )
}

/// The session a clear produced, among the rows now on the agent's pane: the
/// one top-level session of the agent's harness that was not on the pane
/// before the command (`before` holds those, and the agent's own), once the
/// registry has accepted its start with the source the harness names.
///
/// `Ok(None)` waits: no such row yet, a row a status line created, a start
/// with another source, or a harness whose clear announces no session. More
/// than one new session is an error; nothing says which is the agent's.
pub(super) fn cleared_session(
    policy: &ControlPolicy,
    harness: &str,
    rows: &[PaneSession],
    before: &[SessionId],
) -> Result<Option<SessionId>, String> {
    let ClearProof::ChangedSession { start_source } = policy.clear_proof else {
        return Ok(None);
    };
    let mut new = rows.iter().filter(|row| {
        row.view.parent_session_id.is_none()
            && !row.view.id.is_pending()
            && row.view.harness == harness
            && !before.contains(&row.view.id)
    });
    let Some(row) = new.next() else {
        return Ok(None);
    };
    if new.next().is_some() {
        return Err("more than one new session appeared on the pane".into());
    }
    Ok(row
        .start
        .as_ref()
        .filter(|start| start_source.is_none() || start.source.as_deref() == start_source)
        .map(|_| row.view.id.clone()))
}

/// The keys one clear has sent after its command.
#[derive(Debug, Clone, Default)]
pub(super) struct ClearProgress {
    pub worktree_kept: bool,
}

/// Decide a clear's next step once its command was submitted, from the
/// harness's control screen and the session [`cleared_session`] found.
///
/// A harness that announces a new session is done when that session has
/// arrived and, after it, the editor is safe: neither alone is enough. A
/// harness that announces none is done on a fresh, safe editor without the
/// conversation. A recognised dialog other than the worktree choice stops the
/// clear at once; an unrecognised screen is looked at again.
#[must_use]
pub(super) fn decide_clear(
    policy: &ControlPolicy,
    control: &ControlScreen,
    progress: &ClearProgress,
    session: Option<&SessionId>,
) -> Step {
    match &control.dialog {
        ControlDialog::Unknown => Step::Wait("the screen was not recognised"),
        ControlDialog::CurrentWorktree { .. } if progress.worktree_kept => {
            Step::Wait("the worktree choice is still shown after it was answered")
        }
        ControlDialog::CurrentWorktree {
            selected: false, ..
        } => Step::Stuck(
            "the harness asks which worktree to use and the current one is not selected".into(),
        ),
        ControlDialog::CurrentWorktree { accept, .. } => match pane_key(*accept) {
            Some(key) => Step::Key {
                key,
                expect: control.dialog.clone(),
                action: KeyAction::KeepWorktree,
            },
            None => Step::Stuck("the worktree choice needs a key atm cannot send".into()),
        },
        ControlDialog::None => match policy.clear_proof {
            ClearProof::ChangedSession { .. } => match session {
                None => Step::Wait("the harness has not announced a new session"),
                Some(_) if !safe_editor(control) => Step::Wait("the editor is not empty and idle"),
                Some(session) => Step::Ready(Ready::Session(session.clone())),
            },
            ClearProof::ConversationToFresh { .. } if control.conversation => {
                Step::Wait("the conversation is still on screen")
            }
            ClearProof::ConversationToFresh { .. }
                if safe_editor(control)
                    && control.facts.fresh_editor == FreshEditorEvidence::Present =>
            {
                Step::Ready(Ready::Editor)
            }
            ClearProof::ConversationToFresh { .. } => Step::Wait("no fresh editor is shown"),
            _ => Step::Stuck("the harness proves a clear in a way atm does not know".into()),
        },
        _ => Step::Stuck("a dialog opened during the clear".into()),
    }
}
