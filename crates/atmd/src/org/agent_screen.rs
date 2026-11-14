//! The one place in atmd that names an adapter for an agent's screen.
//!
//! Every read takes the capture and the [`PaneBaseline`] recorded for the pane,
//! and answers Unknown (or `None`) unless the harness is positively the pane's
//! foreground program. All of it is pure: no I/O and no awaiting, so it is safe
//! inside a guard predicate of the control actor.
//!
//! The verdict only adds refusals. tmux reports a foreground command name, not
//! a process identity, so `Harness` never permits input on its own: callers
//! still require the adapter's exact screen, the pane tag and focus.

use crate::control::reservation::{CaptureFormat, PaneCapture};
use atm_core::harness_control::{
    ControlPolicy, ControlScreen, HookListEntry, ScreenInput, ScreenTextFormat,
};
use atm_core::{Harness, ScreenCursor, ScreenFacts};

/// What was recorded about a pane's processes when it was created or adopted.
///
/// Held in memory only: after a daemon restart it is derived again, and until
/// then [`PaneBaseline::unproven`] refuses every read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneBaseline {
    root_pid: Option<u32>,
    shell_command: Option<String>,
    harness_command: Option<String>,
}

impl PaneBaseline {
    /// Nothing is known about the pane: every verdict is `None`.
    #[must_use]
    pub fn unproven() -> Self {
        Self {
            root_pid: None,
            shell_command: None,
            harness_command: None,
        }
    }

    /// The pane's root is an idle shell showing `shell_command`; no harness yet.
    #[must_use]
    pub fn shell(root_pid: u32, shell_command: String) -> Self {
        Self {
            root_pid: Some(root_pid),
            shell_command: Some(shell_command),
            harness_command: None,
        }
    }

    /// An adopted pane: its shell's name, if it has a shell, was never seen.
    #[must_use]
    pub fn adopted(root_pid: u32) -> Self {
        Self {
            root_pid: Some(root_pid),
            shell_command: None,
            harness_command: None,
        }
    }

    /// Pin the harness's foreground command name. `None` when the pane is
    /// unproven, `command` is the shell's own name (the two can't be told
    /// apart), or a different name is already pinned: a pin is never replaced.
    #[must_use]
    pub fn with_harness(mut self, command: String) -> Option<Self> {
        self.root_pid?;
        if self.shell_command.as_ref() == Some(&command)
            || self.harness_command.as_ref().is_some_and(|h| *h != command)
        {
            return None;
        }
        self.harness_command = Some(command);
        Some(self)
    }
}

/// Which recorded program is in the pane's foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreground {
    Harness,
    Shell,
}

/// Compare one capture's process facts with the baseline. `None` refuses: the
/// root process is unknown or not the recorded one, the command is unknown, or
/// it is neither recorded name.
#[must_use]
pub fn foreground(
    baseline: &PaneBaseline,
    root_pid: Option<u32>,
    command: Option<&str>,
) -> Option<Foreground> {
    if root_pid? != baseline.root_pid? {
        return None;
    }
    let command = command?;
    if baseline.shell_command.as_deref() == Some(command) {
        Some(Foreground::Shell)
    } else if baseline.harness_command.as_deref() == Some(command) {
        Some(Foreground::Harness)
    } else {
        None
    }
}

/// One harness's screen readers and lifecycle policy, as data.
pub struct AgentScreen {
    harness: Harness,
    policy: &'static ControlPolicy,
    observe: fn(ScreenInput<'_>, Option<ScreenCursor>) -> ScreenFacts,
    staged: fn(ScreenInput<'_>, &str) -> Option<bool>,
    control: fn(ScreenInput<'_>, Option<ScreenCursor>) -> ControlScreen,
    hooks_valid: fn(&[HookListEntry]) -> bool,
}

impl std::fmt::Debug for AgentScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AgentScreen").field(&self.harness).finish()
    }
}

fn no_hook_list(_: &[HookListEntry]) -> bool {
    false
}

static SCREENS: [AgentScreen; 3] = [
    AgentScreen {
        harness: Harness::ClaudeCode,
        policy: &atm_claude_adapter::screen::CONTROL_POLICY,
        observe: atm_claude_adapter::screen::observe_screen,
        staged: atm_claude_adapter::screen::staged_draft_matches,
        control: atm_claude_adapter::screen::observe_control_screen,
        hooks_valid: no_hook_list,
    },
    AgentScreen {
        harness: Harness::Pi,
        policy: &atm_pi_adapter::screen::CONTROL_POLICY,
        observe: atm_pi_adapter::screen::observe_screen,
        staged: atm_pi_adapter::screen::staged_draft_matches,
        control: atm_pi_adapter::screen::observe_control_screen,
        hooks_valid: no_hook_list,
    },
    AgentScreen {
        harness: Harness::Codex,
        policy: &atm_codex_adapter::screen::CONTROL_POLICY,
        observe: atm_codex_adapter::screen::observe_screen,
        staged: atm_codex_adapter::screen::staged_draft_matches,
        control: atm_codex_adapter::screen::observe_control_screen,
        hooks_valid: atm_codex_adapter::screen::validate_hook_list,
    },
];

/// The dispatch for `harness`; `None` refuses (no adapter reads its screen).
#[must_use]
pub fn agent_screen(harness: Harness) -> Option<&'static AgentScreen> {
    SCREENS.iter().find(|screen| screen.harness == harness)
}

impl AgentScreen {
    /// The capture format this harness's adapter reads.
    #[must_use]
    pub fn capture_format(&self) -> CaptureFormat {
        match self.policy.capture {
            ScreenTextFormat::Styled => CaptureFormat::Ansi,
            ScreenTextFormat::Plain => CaptureFormat::Plain,
        }
    }

    #[must_use]
    pub fn policy(&self) -> &'static ControlPolicy {
        self.policy
    }

    #[must_use]
    pub fn foreground(&self, capture: &PaneCapture, baseline: &PaneBaseline) -> Option<Foreground> {
        foreground(
            baseline,
            capture.inspection.pane_root_pid(),
            capture.inspection.foreground_command.as_deref(),
        )
    }

    /// All Unknown unless the capture has this harness's format and the
    /// harness is in the foreground.
    #[must_use]
    pub fn facts(&self, capture: &PaneCapture, baseline: &PaneBaseline) -> ScreenFacts {
        self.harness_input(capture, baseline)
            .map(|input| (self.observe)(input, None))
            .unwrap_or_default()
    }

    /// `None` unless the harness is in the foreground; otherwise the adapter's
    /// exact match of the staged draft with `expected`.
    #[must_use]
    pub fn staged(
        &self,
        capture: &PaneCapture,
        baseline: &PaneBaseline,
        expected: &str,
    ) -> Option<bool> {
        (self.staged)(self.harness_input(capture, baseline)?, expected)
    }

    /// The default (everything Unknown) unless the harness is in the foreground.
    #[must_use]
    pub fn control(&self, capture: &PaneCapture, baseline: &PaneBaseline) -> ControlScreen {
        self.harness_input(capture, baseline)
            .map(|input| (self.control)(input, None))
            .unwrap_or_default()
    }

    /// Before a harness is pinned (a start, an adopt, the pass after a daemon
    /// restart): the command in the foreground and the adapter's read of the
    /// screen, when the pane's root is the recorded one and the command is known
    /// and not the recorded shell's. `None` otherwise, and always once a harness
    /// is pinned. The caller decides whether the screen is recognised and pins
    /// with [`PaneBaseline::with_harness`].
    #[must_use]
    pub fn start_candidate(
        &self,
        capture: &PaneCapture,
        baseline: &PaneBaseline,
    ) -> Option<(String, ControlScreen)> {
        if baseline.harness_command.is_some()
            || capture.inspection.pane_root_pid()? != baseline.root_pid?
        {
            return None;
        }
        let command = capture.inspection.foreground_command.as_ref()?;
        if baseline.shell_command.as_ref() == Some(command) {
            return None;
        }
        let input = self.input(capture)?;
        Some((command.clone(), (self.control)(input, None)))
    }

    /// Whether a complete hook review list is exactly atm's own. False for a
    /// harness without such a list.
    #[must_use]
    pub fn hooks_valid(&self, entries: &[HookListEntry]) -> bool {
        (self.hooks_valid)(entries)
    }

    fn input<'a>(&self, capture: &'a PaneCapture) -> Option<ScreenInput<'a>> {
        (capture.format == self.capture_format()).then(|| match capture.format {
            CaptureFormat::Ansi => ScreenInput::Styled(&capture.text),
            CaptureFormat::Plain => ScreenInput::Plain(&capture.text),
        })
    }

    fn harness_input<'a>(
        &self,
        capture: &'a PaneCapture,
        baseline: &PaneBaseline,
    ) -> Option<ScreenInput<'a>> {
        (self.foreground(capture, baseline) == Some(Foreground::Harness))
            .then(|| self.input(capture))
            .flatten()
    }
}
