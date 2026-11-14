//! Pure decisions of the named-agent lifecycle flows.
//!
//! Nothing here does I/O, awaits or reads a clock: each function maps what one
//! capture showed, plus the little a flow remembers, to the next step. The
//! flows own every effect and re-check each key on the control actor's own
//! capture. No harness is named: all vendor knowledge arrives as the adapter's
//! [`ControlPolicy`] and [`ControlScreen`].

use super::agent_screen::{AgentScreen, Foreground, PaneBaseline};
use crate::control::reservation::{PaneCapture, PaneKey};
use atm_core::harness_control::{
    ControlDialog, ControlKey, ControlPolicy, ControlScreen, SessionArrival,
};
use atm_core::{DialogState, InputState, SessionId, TurnState};
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
